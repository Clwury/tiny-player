use super::*;

impl PlaybackPage {
    pub(in super::super) fn toggle_audio_track_select(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if !self.session.controls_view().audio.enabled() {
            return;
        }
        self.presentation.timeline_presentation.cache_status_open = false;
        self.close_episode_list(cx);
        self.presentation.track_select_open =
            if self.presentation.track_select_open == Some(PlaybackTrackKind::Audio) {
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
        if !self.session.controls_view().subtitles.enabled() {
            return;
        }
        self.presentation.timeline_presentation.cache_status_open = false;
        self.close_episode_list(cx);
        self.presentation.track_select_open =
            if self.presentation.track_select_open == Some(PlaybackTrackKind::Subtitle) {
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
        self.dispatch_control(PlaybackIntent::SelectAudio(track_index), cx);
    }

    pub(in super::super) fn select_subtitle_track(
        &mut self,
        track: Option<PlaybackTrack>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let update = self.dispatch_control(PlaybackIntent::SelectSubtitle(track), cx);
        if update.clear_subtitle {
            defer_drop_subtitle(&mut self.presentation.subtitle, window);
        }
    }

    pub(in crate::player::page) fn remember_track_choice(
        &self,
        kind: PlaybackTrackKind,
        cx: &mut Context<Self>,
    ) {
        let Some(update) = self.session.track_preference_update(
            kind,
            &self.emby.item_id,
            &self.emby.media_source_id,
        ) else {
            return;
        };
        crate::player::PlaybackTrackPreferences::remember(
            &self.emby.server,
            &update.keys,
            kind,
            update.track,
            cx,
        );
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
            page.session.queue.queue_mut().items[0].item_id = "grouped-episode".into();
            page.emby.item_id = "resolved-episode".into();
            page.emby.media_source_id = "resolved-source".into();
            page.session.source_mut().tracks.audio = vec![PlaybackTrack::new(1, "Japanese", false)];
            page.session.source_mut().tracks.subtitles =
                vec![PlaybackTrack::new(9, "Chinese Simplified", false)];
            page.session.source_mut().tracks.selected_audio_stream_index = Some(1);
            page.session
                .source_mut()
                .tracks
                .selected_subtitle_stream_index = Some(9);
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
                    Some(SavedTrackChoice::from_track(
                        page.session.source_view().tracks.audio.first()
                    ))
                );
                assert_eq!(
                    saved.subtitle,
                    Some(SavedTrackChoice::from_track(
                        page.session.source_view().tracks.subtitles.first()
                    ))
                );
            }
            page.session
                .source_mut()
                .tracks
                .selected_subtitle_stream_index = None;
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
            page.session.source_mut().tracks.audio = vec![PlaybackTrack::new(1, "Japanese", false)];
            page.session.source_mut().tracks.subtitles =
                vec![PlaybackTrack::new(9, "Chinese Simplified", false)];
            page.session.source_mut().tracks.selected_audio_stream_index = Some(1);
            page.session
                .source_mut()
                .tracks
                .selected_subtitle_stream_index = Some(9);
            page.remember_track_choice(PlaybackTrackKind::Audio, cx);
            page.remember_track_choice(PlaybackTrackKind::Subtitle, cx);
        });
        for backend_missing in [true, false] {
            cx.update(|window, cx| {
                page.update(cx, |page, cx| {
                    if !backend_missing {
                        let state = std::rc::Rc::new(std::cell::RefCell::new(
                            crate::player::backend::test_support::FakeState {
                                fail_commands: true,
                                ..Default::default()
                            },
                        ));
                        page.video = crate::player::backend::test_support::adapter(state, false);
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
                    page.presentation.subtitle.images.update(Some(&cue));
                    page.presentation.subtitle.active = Some(cue.clone());
                    let rendered = page
                        .presentation
                        .subtitle
                        .images
                        .get(&image)
                        .unwrap()
                        .clone();
                    page.select_audio_track(None, window, cx);
                    page.select_subtitle_track(None, window, cx);
                    assert_eq!(
                        page.session
                            .source_view()
                            .tracks
                            .selected_audio_stream_index,
                        Some(1)
                    );
                    assert_eq!(
                        page.session
                            .source_view()
                            .tracks
                            .selected_subtitle_stream_index,
                        Some(9)
                    );
                    assert_eq!(&before, cx.global::<PlaybackTrackPreferences>());
                    assert_eq!(page.presentation.subtitle.active, Some(cue));
                    assert!(Arc::ptr_eq(
                        page.presentation.subtitle.images.get(&image).unwrap(),
                        &rendered
                    ));
                });
            });
        }
    }
}
