use super::*;

impl PlaybackPage {
    pub(in super::super) fn toggle_audio_track_select(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if self.tracks.audio.is_empty() && self.tracks.selected_audio_stream_index.is_none() {
            return;
        }
        self.timeline.cache_status_open = false;
        self.close_episode_list(cx);
        self.tracks.open = if self.tracks.open == Some(PlaybackTrackKind::Audio) {
            None
        } else {
            Some(PlaybackTrackKind::Audio)
        };
        cx.notify();
    }

    pub(in super::super) fn toggle_subtitle_track_select(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if self.tracks.subtitles.is_empty() && self.tracks.selected_subtitle_stream_index.is_none()
        {
            return;
        }
        self.timeline.cache_status_open = false;
        self.close_episode_list(cx);
        self.tracks.open = if self.tracks.open == Some(PlaybackTrackKind::Subtitle) {
            None
        } else {
            Some(PlaybackTrackKind::Subtitle)
        };
        cx.notify();
    }

    pub(in super::super) fn select_audio_track(
        &mut self,
        track_index: Option<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position_seconds = self
            .timeline
            .progress_drag_position
            .or(self.timeline.position)
            .unwrap_or(0.0);
        let previous_audio = self.tracks.selected_audio_stream_index;
        self.tracks.selected_audio_stream_index = track_index;
        self.tracks.open = None;
        if track_index.is_some() {
            self.timeline.buffering = self.timeline.loaded;
        }

        let command_succeeded = if let Some(backend) = self.video.owner_mut() {
            match backend.command(BackendCommand::SetAudioTrack {
                track_index,
                position_seconds,
            }) {
                Ok(()) => true,
                Err(error) => {
                    self.tracks.selected_audio_stream_index = previous_audio;
                    self.timeline.buffering = false;
                    self.error_message = Some(format!("切换轨道失败：{error}").into());
                    false
                }
            }
        } else {
            self.tracks.selected_audio_stream_index = previous_audio;
            self.timeline.buffering = false;
            false
        };
        if command_succeeded {
            self.remember_track_choice(PlaybackTrackKind::Audio, cx);
            self.report_playback_progress(true);
        }
        cx.notify();
    }

    pub(in super::super) fn select_subtitle_track(
        &mut self,
        track: Option<PlaybackTrack>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position_seconds = self
            .timeline
            .progress_drag_position
            .or(self.timeline.position)
            .unwrap_or(0.0);
        let previous_audio = self.tracks.selected_audio_stream_index;
        let previous_subtitle = self.tracks.selected_subtitle_stream_index;
        let mut previous_active_subtitle = self.subtitle.active.take();
        self.tracks.selected_subtitle_stream_index = track.as_ref().map(|track| track.stream_index);
        self.tracks.open = None;
        if track.is_some() {
            self.timeline.buffering = self.timeline.loaded;
        }

        let command_succeeded = if let Some(backend) = self.video.owner_mut() {
            match backend.command(BackendCommand::SetSubtitleTrack {
                track,
                position_seconds,
            }) {
                Ok(()) => true,
                Err(error) => {
                    self.tracks.selected_audio_stream_index = previous_audio;
                    self.tracks.selected_subtitle_stream_index = previous_subtitle;
                    self.subtitle.active = previous_active_subtitle.take();
                    self.timeline.buffering = false;
                    self.error_message = Some(format!("切换轨道失败：{error}").into());
                    false
                }
            }
        } else {
            self.tracks.selected_audio_stream_index = previous_audio;
            self.tracks.selected_subtitle_stream_index = previous_subtitle;
            self.subtitle.active = previous_active_subtitle.take();
            self.timeline.buffering = false;
            false
        };
        if command_succeeded {
            self.remember_track_choice(PlaybackTrackKind::Subtitle, cx);
            self.report_playback_progress(true);
            defer_drop_subtitle(&mut self.subtitle, window);
        }
        cx.notify();
    }

    pub(in crate::player::page) fn remember_track_choice(
        &self,
        kind: PlaybackTrackKind,
        cx: &mut Context<Self>,
    ) {
        use crate::player::{PlaybackTrackPreferenceKey, PlaybackTrackPreferences};

        let (index, tracks) = match kind {
            PlaybackTrackKind::Audio => {
                (self.tracks.selected_audio_stream_index, &self.tracks.audio)
            }
            PlaybackTrackKind::Subtitle => (
                self.tracks.selected_subtitle_stream_index,
                &self.tracks.subtitles,
            ),
        };
        let track = index.and_then(|index| tracks.iter().find(|track| track.stream_index == index));
        if index.is_some() && track.is_none() {
            return;
        }
        let mut keys = vec![self.track_preference_key.clone()];
        // Resume cards and grouped episode lists can address the same version
        // through different item IDs. Restore the choice through either route.
        for item_id in std::iter::once(self.emby.item_id.as_str())
            .chain(self.queue.current().map(|item| item.item_id.as_str()))
        {
            for source_id in [
                &self.track_preference_key.media_source_id,
                &self.emby.media_source_id,
            ] {
                let key = PlaybackTrackPreferenceKey {
                    item_id: item_id.to_string(),
                    media_source_id: source_id.clone(),
                };
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        PlaybackTrackPreferences::remember(&self.emby.server, &keys, kind, track, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::{PlaybackTrackPreferenceKey, PlaybackTrackPreferences, SavedTrackChoice};

    #[gpui::test]
    fn saved_player_choices_restore_for_grouped_and_resolved_item_ids(
        cx: &mut gpui::TestAppContext,
    ) {
        let (page, cx) = episodes::tests::playback_window(cx);
        page.update(cx, |page, cx| {
            page.queue.items[0].item_id = "grouped-episode".into();
            page.emby.item_id = "resolved-episode".into();
            page.emby.media_source_id = "resolved-source".into();
            page.tracks.audio = vec![PlaybackTrack::new(1, "Japanese", false)];
            page.tracks.subtitles = vec![PlaybackTrack::new(9, "Chinese Simplified", false)];
            page.tracks.selected_audio_stream_index = Some(1);
            page.tracks.selected_subtitle_stream_index = Some(9);
            page.remember_track_choice(PlaybackTrackKind::Audio, cx);
            page.remember_track_choice(PlaybackTrackKind::Subtitle, cx);
            for item_id in ["episode-0", "grouped-episode", "resolved-episode"] {
                let key = PlaybackTrackPreferenceKey {
                    item_id: item_id.into(),
                    media_source_id: "source-0".into(),
                };
                let saved = PlaybackTrackPreferences::get(&page.emby.server, &key, cx);
                assert_eq!(
                    saved.audio,
                    Some(SavedTrackChoice::from_track(page.tracks.audio.first()))
                );
                assert_eq!(
                    saved.subtitle,
                    Some(SavedTrackChoice::from_track(page.tracks.subtitles.first()))
                );
            }
            page.tracks.selected_subtitle_stream_index = None;
            page.remember_track_choice(PlaybackTrackKind::Subtitle, cx);
            let saved = PlaybackTrackPreferences::get(
                &page.emby.server,
                &PlaybackTrackPreferenceKey {
                    item_id: "resolved-episode".into(),
                    media_source_id: "resolved-source".into(),
                },
                cx,
            );
            assert!(saved.audio.is_some());
            assert_eq!(saved.subtitle, Some(SavedTrackChoice::Off));
        });
    }

    #[gpui::test]
    fn rejected_track_switches_do_not_overwrite_saved_choices(cx: &mut gpui::TestAppContext) {
        let (page, cx) = episodes::tests::playback_window(cx);
        page.update(cx, |page, cx| {
            page.tracks.audio = vec![PlaybackTrack::new(1, "Japanese", false)];
            page.tracks.subtitles = vec![PlaybackTrack::new(9, "Chinese Simplified", false)];
            page.tracks.selected_audio_stream_index = Some(1);
            page.tracks.selected_subtitle_stream_index = Some(9);
            page.remember_track_choice(PlaybackTrackKind::Audio, cx);
            page.remember_track_choice(PlaybackTrackKind::Subtitle, cx);
        });
        for backend_missing in [true, false] {
            cx.update(|window, cx| {
                page.update(cx, |page, cx| {
                    if !backend_missing {
                        page.video = ShutdownOrder::new(
                            Some(PlaybackBackend::Ffmpeg(FfmpegBackend::new().unwrap())),
                            None,
                        );
                    }
                    let before = cx.global::<PlaybackTrackPreferences>().clone();
                    let image = tiny_playback::SharedBgraImage::new(
                        tiny_playback::BgraImage::new(vec![1, 2, 3, 128], 1, 1).unwrap(),
                    );
                    let cue = BackendSubtitleCue {
                        text: String::new(),
                        bitmaps: vec![BackendSubtitleBitmap {
                            image: image.clone(),
                            x: 0,
                            y: 0,
                            width: 1,
                            height: 1,
                            canvas_width: 1920,
                            canvas_height: 1080,
                        }],
                        start_nsecs: 0,
                        end_nsecs: 1_000_000_000,
                    };
                    page.subtitle.images.update(Some(&cue));
                    page.subtitle.active = Some(cue.clone());
                    let rendered = page.subtitle.images.get(&image).unwrap().clone();
                    page.select_audio_track(None, window, cx);
                    page.select_subtitle_track(None, window, cx);
                    assert_eq!(page.tracks.selected_audio_stream_index, Some(1));
                    assert_eq!(page.tracks.selected_subtitle_stream_index, Some(9));
                    assert_eq!(&before, cx.global::<PlaybackTrackPreferences>());
                    assert_eq!(page.subtitle.active, Some(cue));
                    assert!(Arc::ptr_eq(
                        page.subtitle.images.get(&image).unwrap(),
                        &rendered
                    ));
                });
            });
        }
    }
}
