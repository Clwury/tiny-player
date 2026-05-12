use super::*;

pub(super) fn dovi_metadata_from_frame(frame: *mut ffi::AVFrame) -> Option<DoviFrameMetadata> {
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
    match DoviFrameMetadata::from_rpu_payload(data)
        .or_else(|_| DoviFrameMetadata::from_unspec62_nalu(data))
    {
        Ok(metadata) => Some(metadata),
        Err(error) => {
            tracing::debug!(%error, "failed to parse Dolby Vision RPU from decoded frame side data");
            None
        }
    }
}

#[derive(Default)]
pub(super) struct DoviMetadataQueue {
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
