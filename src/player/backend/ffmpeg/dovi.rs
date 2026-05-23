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
    pub(super) fn observe_video_packet(&mut self, packet: &AvPacket, stream: StreamInfo) -> bool {
        self.state.observe_packet(packet, stream)
    }

    pub(super) fn observe_video_packet_metadata(
        &mut self,
        packet: &AvPacket,
        stream: StreamInfo,
        metadata: DoviFrameMetadata,
    ) {
        self.state.observe_packet_metadata(packet, stream, metadata);
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
    stream_profile5: bool,
    stream_profile_checked: bool,
    reused_profile5_logged: bool,
    inherited_profile5_logged: bool,
    ffmpeg_non_profile5_logged: bool,
}

impl DoviMetadataState {
    pub(super) fn observe_packet(&mut self, packet: &AvPacket, stream: StreamInfo) -> bool {
        self.observe_stream_config(stream);
        let metadata = self.queue.observe_packet(packet, stream);
        if metadata
            .as_ref()
            .is_some_and(DoviFrameMetadata::is_profile5)
        {
            self.stream_profile5 = true;
        }
        metadata.is_some()
    }

    pub(super) fn observe_packet_metadata(
        &mut self,
        packet: &AvPacket,
        stream: StreamInfo,
        metadata: DoviFrameMetadata,
    ) {
        self.observe_stream_config(stream);
        if metadata.is_profile5() {
            self.stream_profile5 = true;
        }
        self.queue.observe_packet_metadata(packet, stream, metadata);
    }

    pub(super) fn metadata_for_frame(
        &mut self,
        frame: *mut ffi::AVFrame,
        pts: FramePts,
    ) -> Option<DoviFrameMetadata> {
        if let Some(metadata) = ffmpeg_dovi_metadata_from_frame(frame) {
            if metadata.is_profile5() {
                self.stream_profile5 = true;
            } else if !self.ffmpeg_non_profile5_logged {
                self.ffmpeg_non_profile5_logged = true;
                tracing::debug!(
                    "observed non-Profile 5 FFmpeg Dolby Vision frame metadata; keeping packet RPU metadata when available"
                );
            }
        }
        let allow_ordered_fallback = self.stream_profile5
            || self
                .last_profile5
                .as_ref()
                .is_some_and(DoviFrameMetadata::is_profile5);
        let metadata = dovi_metadata_from_frame(frame)
            .or_else(|| self.queue.take_for_frame(pts, allow_ordered_fallback));
        self.resolve(metadata)
    }

    pub(super) fn discard_frame(&mut self, pts: FramePts) {
        let _ = self.queue.take_for_frame(pts, false);
    }

    pub(super) fn clear(&mut self) {
        self.queue.clear();
        if !self.stream_profile5 {
            self.last_profile5 = None;
        }
        self.reused_profile5_logged = false;
        self.inherited_profile5_logged = false;
        self.ffmpeg_non_profile5_logged = false;
    }

    fn resolve(&mut self, metadata: Option<DoviFrameMetadata>) -> Option<DoviFrameMetadata> {
        if let Some(mut metadata) = metadata {
            if metadata.is_profile5() {
                self.stream_profile5 = true;
            } else if self.stream_profile5 {
                metadata.profile5 = true;
                if !self.inherited_profile5_logged {
                    self.inherited_profile5_logged = true;
                    tracing::debug!(
                        profile = metadata.profile,
                        "inheriting Dolby Vision Profile 5 stream state for RPU metadata"
                    );
                }
            }
            if metadata.is_profile5() {
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

    fn observe_stream_config(&mut self, stream: StreamInfo) {
        if self.stream_profile_checked {
            return;
        }
        self.stream_profile_checked = true;
        if stream_dovi_profile5(stream) {
            self.stream_profile5 = true;
            tracing::debug!("detected Dolby Vision Profile 5 stream configuration");
        }
    }
}

fn stream_dovi_profile5(stream: StreamInfo) -> bool {
    if stream.stream.is_null() {
        return false;
    }
    let codecpar = unsafe { (*stream.stream).codecpar };
    if codecpar.is_null() {
        return false;
    }
    let side_data = unsafe {
        ffi::av_packet_side_data_get(
            (*codecpar).coded_side_data,
            (*codecpar).nb_coded_side_data,
            ffi::AVPacketSideDataType::AV_PKT_DATA_DOVI_CONF,
        )
    };
    if side_data.is_null() {
        return false;
    }
    let (data, size) = unsafe { ((*side_data).data, (*side_data).size) };
    if data.is_null() || size == 0 {
        return false;
    }
    dovi_config_is_profile5(unsafe { slice::from_raw_parts(data, size) })
}

fn dovi_config_is_profile5(data: &[u8]) -> bool {
    data.get(2).is_some_and(|profile| *profile == 5)
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
    pub(super) fn observe_packet(
        &mut self,
        packet: &AvPacket,
        stream: StreamInfo,
    ) -> Option<DoviFrameMetadata> {
        if stream.codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
            return None;
        }
        let data = packet.data()?;
        let metadata = match extract_dovi_metadata(&mut self.extractor, data) {
            Ok(Some(metadata)) => metadata,
            Ok(None) => return None,
            Err(error) => {
                tracing::trace!(%error, "ignored non-Dolby Vision RPU candidate in FFmpeg packet");
                return None;
            }
        };
        self.observe_packet_metadata(packet, stream, metadata.clone());
        Some(metadata)
    }

    pub(super) fn observe_packet_metadata(
        &mut self,
        packet: &AvPacket,
        stream: StreamInfo,
        metadata: DoviFrameMetadata,
    ) {
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

    pub(super) fn take_for_frame(
        &mut self,
        pts: FramePts,
        allow_ordered_fallback: bool,
    ) -> Option<DoviFrameMetadata> {
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
            .is_some_and(|entry| entry.metadata.is_profile5() || allow_ordered_fallback)
        {
            let entry_pts = self.entries.front().and_then(|entry| entry.pts);
            let distance = entry_pts.map(|entry_pts| pts_distance(entry_pts, pts));
            tracing::trace!(
                frame_pts = pts.nsecs,
                entry_pts = ?entry_pts.map(|pts| pts.nsecs),
                distance_nsecs = ?distance,
                queue_len = self.entries.len(),
                allow_ordered_fallback,
                "using ordered Dolby Vision RPU metadata after timestamp miss"
            );
            return self.entries.pop_front().map(|entry| entry.metadata);
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
    if starts_with_annex_b_start_code(data) {
        return extractor.extract_from_hevc_access_unit(data, HevcStreamFormat::ByteStream);
    }

    let mut last_error = None;
    for length_size in [4, 3, 2, 1] {
        match extractor
            .extract_from_hevc_access_unit(data, HevcStreamFormat::LengthPrefixed { length_size })
        {
            Ok(metadata) => return Ok(metadata),
            Err(error) => last_error = Some(error),
        }
    }
    if has_annex_b_start_code(data) {
        match extractor.extract_from_hevc_access_unit(data, HevcStreamFormat::ByteStream) {
            Ok(Some(metadata)) => return Ok(Some(metadata)),
            Ok(None) => {}
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

fn starts_with_annex_b_start_code(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dovi_state_reuses_last_profile5_metadata_when_frame_metadata_is_missing() {
        let mut state = DoviMetadataState::default();
        let metadata = DoviFrameMetadata {
            profile: 5,
            profile5: true,
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
        assert!(metadata.is_profile5());
        assert_eq!(metadata.rpu_payload, payload);
    }

    #[test]
    fn dovi_state_does_not_reuse_non_profile5_metadata() {
        let mut state = DoviMetadataState::default();
        let metadata = DoviFrameMetadata {
            profile: 8,
            profile5: false,
            rpu_nalu: vec![1],
            rpu_payload: vec![2],
        };

        assert_eq!(
            state.resolve(Some(metadata)),
            Some(DoviFrameMetadata {
                profile: 8,
                profile5: false,
                rpu_nalu: vec![1],
                rpu_payload: vec![2],
            })
        );
        assert_eq!(state.resolve(None), None);
    }

    #[test]
    fn dovi_state_inherits_profile5_stream_state_for_later_rpus() {
        let mut state = DoviMetadataState::default();
        let first = DoviFrameMetadata {
            profile: 5,
            profile5: true,
            rpu_nalu: vec![1],
            rpu_payload: vec![2],
        };
        let later = DoviFrameMetadata {
            profile: 0,
            profile5: false,
            rpu_nalu: vec![3],
            rpu_payload: vec![4],
        };

        assert!(state.resolve(Some(first)).unwrap().is_profile5());
        let inherited = state.resolve(Some(later)).unwrap();

        assert!(inherited.is_profile5());
        assert_eq!(inherited.profile, 0);
        assert_eq!(state.resolve(None), Some(inherited));
    }

    #[test]
    fn dovi_state_preserves_profile5_stream_state_across_seek_clear() {
        let mut state = DoviMetadataState::default();
        let first = DoviFrameMetadata {
            profile: 5,
            profile5: true,
            rpu_nalu: vec![1],
            rpu_payload: vec![2],
        };
        let after_seek = DoviFrameMetadata {
            profile: 0,
            profile5: false,
            rpu_nalu: vec![3],
            rpu_payload: vec![4],
        };

        assert!(state.resolve(Some(first)).unwrap().is_profile5());
        state.clear();

        assert!(state.resolve(Some(after_seek)).unwrap().is_profile5());
    }

    #[test]
    fn dovi_state_preserves_last_profile5_metadata_across_seek_clear() {
        let mut state = DoviMetadataState::default();
        let metadata = DoviFrameMetadata {
            profile: 5,
            profile5: true,
            rpu_nalu: vec![1],
            rpu_payload: vec![2],
        };

        assert_eq!(
            state.resolve(Some(metadata.clone())),
            Some(metadata.clone())
        );
        state.clear();

        assert_eq!(state.resolve(None), Some(metadata));
    }

    #[test]
    fn dovi_queue_uses_profile5_ordered_fallback_after_timestamp_miss() {
        let mut queue = DoviMetadataQueue::default();
        let metadata = DoviFrameMetadata {
            profile: 5,
            profile5: true,
            rpu_nalu: vec![1],
            rpu_payload: vec![2],
        };
        queue.entries.push_back(DoviMetadataEntry {
            pts: Some(FramePts { nsecs: 1_000 }),
            metadata: metadata.clone(),
        });

        assert_eq!(
            queue.take_for_frame(
                FramePts {
                    nsecs: 1_000_000_000
                },
                false
            ),
            Some(metadata)
        );
        assert!(queue.entries.is_empty());
    }

    #[test]
    fn dovi_queue_uses_stream_profile_ordered_fallback_after_timestamp_miss() {
        let mut queue = DoviMetadataQueue::default();
        let metadata = DoviFrameMetadata {
            profile: 0,
            profile5: false,
            rpu_nalu: vec![1],
            rpu_payload: vec![2],
        };
        queue.entries.push_back(DoviMetadataEntry {
            pts: Some(FramePts { nsecs: 1_000 }),
            metadata: metadata.clone(),
        });

        assert_eq!(
            queue.take_for_frame(
                FramePts {
                    nsecs: 1_000_000_000
                },
                true
            ),
            Some(metadata)
        );
        assert!(queue.entries.is_empty());
    }

    #[test]
    fn dovi_queue_does_not_order_fallback_non_profile5_after_timestamp_miss() {
        let mut queue = DoviMetadataQueue::default();
        queue.entries.push_back(DoviMetadataEntry {
            pts: Some(FramePts { nsecs: 1_000 }),
            metadata: DoviFrameMetadata {
                profile: 8,
                profile5: false,
                rpu_nalu: vec![1],
                rpu_payload: vec![2],
            },
        });

        assert_eq!(
            queue.take_for_frame(
                FramePts {
                    nsecs: 1_000_000_000
                },
                false
            ),
            None
        );
        assert_eq!(queue.entries.len(), 1);
    }

    #[test]
    fn dovi_config_detects_profile5() {
        assert!(dovi_config_is_profile5(&[1, 0, 5, 6, 1, 0, 1, 0, 0]));
        assert!(!dovi_config_is_profile5(&[1, 0, 8, 6, 1, 0, 1, 1, 0]));
        assert!(!dovi_config_is_profile5(&[1, 0]));
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
