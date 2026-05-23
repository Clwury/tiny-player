use super::*;

pub(super) struct SubtitlePipeline {
    stream: Option<StreamInfo>,
    decoder: Option<Decoder>,
    filter: Option<PgsFrameMergeBitstreamFilter>,
    filtered_packet: Option<AvPacket>,
    external_cues: Vec<BackendSubtitleCue>,
    cues: VecDeque<BackendSubtitleCue>,
    active: Option<BackendSubtitleCue>,
    needs_prefetch: bool,
}

pub(super) struct SubtitleDecodeContext<'a> {
    pub(super) current_start_position_nsecs: u64,
    pub(super) playback_timeline_origin_nsecs: Option<u64>,
    pub(super) control: &'a FfmpegControl,
    pub(super) audio_output: Option<&'a AudioOutput>,
    pub(super) session_id: PlaybackSessionId,
    pub(super) event_tx: &'a Sender<BackendEvent>,
}

impl SubtitlePipeline {
    pub(super) fn new(
        stream: Option<StreamInfo>,
        decoder: Option<Decoder>,
        source: &FfmpegPlaybackInput,
        start_position_nsecs: u64,
    ) -> std::result::Result<Self, String> {
        let filter = match stream {
            Some(stream) => PgsFrameMergeBitstreamFilter::new(stream)?,
            None => None,
        };
        let filtered_packet = if filter.is_some() {
            Some(AvPacket::new()?)
        } else {
            None
        };
        let external_cues = load_external_subtitle_cue_list(
            &source.selected_tracks,
            source.http_headers.as_slice(),
        )?;
        let cues = subtitle_cue_queue_from_external(&external_cues, start_position_nsecs);

        Ok(Self {
            stream,
            decoder,
            filter,
            filtered_packet,
            external_cues,
            cues,
            active: None,
            needs_prefetch: subtitle_needs_prefetch(stream),
        })
    }

    pub(super) fn switch_tracks(
        &mut self,
        source: &FfmpegPlaybackInput,
        stream_catalog: &StreamCatalog,
        video_size: Option<RenderSize>,
        start_position_nsecs: u64,
    ) -> std::result::Result<(), String> {
        let stream = select_subtitle_stream_for_selection_from_catalog(
            &source.selected_tracks,
            stream_catalog,
        )?;
        let decoder = open_subtitle_decoder(stream, video_size)?;
        let filter = match stream {
            Some(stream) => PgsFrameMergeBitstreamFilter::new(stream)?,
            None => None,
        };
        let filtered_packet = if filter.is_some() {
            Some(AvPacket::new()?)
        } else {
            None
        };
        let external_cues = load_external_subtitle_cue_list(
            &source.selected_tracks,
            source.http_headers.as_slice(),
        )?;

        self.stream = stream;
        self.decoder = decoder;
        self.filter = filter;
        self.filtered_packet = filtered_packet;
        self.external_cues = external_cues;
        self.reset_cues_for_position(start_position_nsecs);
        self.needs_prefetch = subtitle_needs_prefetch(stream);
        Ok(())
    }

    pub(super) fn matches_stream_index(&self, stream_index: i32) -> bool {
        self.decoder
            .as_ref()
            .is_some_and(|decoder| stream_index == decoder.stream_index)
    }

    pub(super) fn needs_prefetch(&self) -> bool {
        self.needs_prefetch
    }

    pub(super) fn flush_decode_state(&mut self) {
        if let Some(decoder) = &self.decoder {
            decoder.flush_buffers();
        }
        if let Some(filter) = self.filter.as_mut() {
            filter.flush();
        }
        if let Some(packet) = self.filtered_packet.as_mut() {
            packet.unref();
        }
    }

    pub(super) fn reset_cues_for_position(&mut self, start_position_nsecs: u64) {
        self.cues = subtitle_cue_queue_from_external(&self.external_cues, start_position_nsecs);
        self.active = None;
    }

    pub(super) fn refresh_timeline_origin(
        &mut self,
        playback_timeline_origin_nsecs: &mut Option<u64>,
        video_clock: &TimestampMapper,
    ) {
        refresh_playback_timeline_origin(
            playback_timeline_origin_nsecs,
            video_clock,
            self.stream,
            &mut self.cues,
        );
    }

    pub(super) fn decode_packet(
        &mut self,
        packet: &mut AvPacket,
        context: SubtitleDecodeContext<'_>,
    ) -> std::result::Result<(), String> {
        let decoder = self
            .decoder
            .as_ref()
            .expect("subtitle decoder exists for subtitle stream packet");
        let stream = self.stream.expect("subtitle stream exists with decoder");
        if let Some(filter) = self.filter.as_mut() {
            filter.send_packet(packet.as_mut_ptr())?;
            let filtered_packet = self
                .filtered_packet
                .as_mut()
                .expect("filtered subtitle packet exists with subtitle filter");
            loop {
                if !filter.receive_packet(filtered_packet)? {
                    break;
                }
                decode_subtitle_packet_into_queue(
                    decoder,
                    stream,
                    filtered_packet,
                    context.current_start_position_nsecs,
                    context.playback_timeline_origin_nsecs,
                    &mut self.cues,
                    context.control,
                )?;
                if let Some(output) = context.audio_output {
                    update_subtitle_overlay(
                        output.played_timeline_nsecs(),
                        &mut self.cues,
                        &mut self.active,
                        context.session_id,
                        context.event_tx,
                    );
                }
                filtered_packet.unref();
            }
            Ok(())
        } else {
            decode_subtitle_packet_into_queue(
                decoder,
                stream,
                packet,
                context.current_start_position_nsecs,
                context.playback_timeline_origin_nsecs,
                &mut self.cues,
                context.control,
            )?;
            if let Some(output) = context.audio_output {
                self.update_overlay_from_audio_clock(output, context.session_id, context.event_tx);
            }
            Ok(())
        }
    }

    pub(super) fn update_overlay_from_audio_clock(
        &mut self,
        output: &AudioOutput,
        session_id: PlaybackSessionId,
        event_tx: &Sender<BackendEvent>,
    ) {
        self.update_overlay(output.played_timeline_nsecs(), session_id, event_tx);
    }

    pub(super) fn update_overlay(
        &mut self,
        timeline_nsecs: u64,
        session_id: PlaybackSessionId,
        event_tx: &Sender<BackendEvent>,
    ) {
        update_subtitle_overlay(
            timeline_nsecs,
            &mut self.cues,
            &mut self.active,
            session_id,
            event_tx,
        );
    }
}

fn subtitle_needs_prefetch(stream: Option<StreamInfo>) -> bool {
    stream.is_some_and(|stream| stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE)
}
