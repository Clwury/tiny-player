use super::*;
use crate::player::PlaybackLanguagePreferences;

fn detail_icon_button(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    enabled: bool,
    cx: &gpui::App,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);
    div()
        .id(id)
        .group(id)
        .debug_selector(move || id.into())
        .role(gpui::Role::Button)
        .aria_label(label)
        .tooltip(move |_, cx| text_tooltip(label, cx))
        .flex()
        .flex_none()
        .size(px(32.0))
        .items_center()
        .justify_center()
        .child(
            svg()
                .path(icon)
                .size(px(18.0))
                .text_color(theme.foreground)
                .when(enabled, |this| {
                    this.group_hover(id, move |style| style.text_color(theme.accent_text))
                }),
        )
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .when(enabled, |this| this.cursor_pointer())
        .when(!enabled, |this| this.cursor_default().opacity(0.55))
}

impl HomeContent {
    fn render_series_actions_menu(
        &self,
        detail: &SeriesDetailState,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let data = self.effective_user_data(
            &detail.series_id,
            detail
                .item
                .as_ref()
                .and_then(|item| item.user_data.as_ref()),
        );
        let favorite = data.is_some_and(|data| data.is_favorite);
        let played = data.is_some_and(|data| data.played);
        let enabled = !self.detail_user_data_pending();
        div()
            .id("series-detail-actions-menu")
            .debug_selector(|| "series-detail-actions-menu".into())
            .absolute()
            .top(px(34.0))
            .left_0()
            .w(px(164.0))
            .p_1()
            .flex()
            .flex_col()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(theme.input_border)
            .bg(theme.dialog_background)
            .shadow_lg()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .when_some(detail.action_menu_focus.as_ref(), |this, focus| {
                this.track_focus(focus)
            })
            .on_key_down(cx.listener(|page, event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    if let Some(detail) = page.series_detail.as_mut() {
                        detail.open_select = None;
                    }
                    window.blur(cx);
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .children(
                [
                    (
                        "series-detail-series-favorite",
                        if favorite {
                            "取消剧集收藏"
                        } else {
                            "收藏剧集"
                        },
                        if favorite {
                            "icons/heart-filled.svg"
                        } else {
                            "icons/heart.svg"
                        },
                        true,
                    ),
                    (
                        "series-detail-series-played",
                        if played {
                            "标记为未观看"
                        } else {
                            "标记为已观看"
                        },
                        if played {
                            "icons/circle-check-filled.svg"
                        } else {
                            "icons/circle-check.svg"
                        },
                        false,
                    ),
                ]
                .into_iter()
                .map(|(id, label, icon, favorite_action)| {
                    div()
                        .id(id)
                        .debug_selector(move || id.into())
                        .role(gpui::Role::MenuItem)
                        .aria_label(label)
                        .h(px(30.0))
                        .px_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .rounded_md()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(
                            svg()
                                .path(icon)
                                .size(px(16.0))
                                .flex_none()
                                .text_color(theme.foreground),
                        )
                        .child(label)
                        .when(enabled, |this| {
                            this.cursor_pointer()
                                .hover(move |style| style.bg(theme.secondary_hover))
                                .on_click(cx.listener(move |page, _, window, cx| {
                                    cx.stop_propagation();
                                    window.blur(cx);
                                    if favorite_action {
                                        page.toggle_series_favorite(cx);
                                    } else {
                                        page.toggle_detail_played(true, cx);
                                    }
                                }))
                        })
                        .when(!enabled, |this| this.opacity(0.55))
                }),
            )
    }

    pub(super) fn render_series_detail_controls(
        &self,
        detail: &SeriesDetailState,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let video_label = detail.selected_media_source_label();
        let subtitle_language = PlaybackLanguagePreferences::get(cx).subtitle;
        let subtitle_label = detail.selected_subtitle_label(subtitle_language);
        let play = cx.listener(Self::play_selected_media);
        let toggle_video = cx.listener(Self::toggle_series_media_source_select);
        let toggle_subtitle = cx.listener(Self::toggle_series_subtitle_select);
        let toggle_favorite = cx.listener(Self::toggle_detail_favorite);
        let selected_item = detail.selected_playback_item();
        let selected_data = selected_item
            .and_then(|item| self.effective_user_data(&item.id, item.user_data.as_ref()));
        let favorite = selected_data.is_some_and(|data| data.is_favorite);
        let played = selected_data.is_some_and(|data| data.played);
        let actions_enabled = selected_item.is_some() && !self.detail_user_data_pending();
        let media_sources = detail.selected_media_sources().unwrap_or_default();
        let source_count = media_sources.len();
        let selected_source_index = detail.selected_media_source_index();
        let subtitle_streams = detail
            .selected_media_source()
            .map(|source| source.subtitle_streams())
            .unwrap_or_default();
        let subtitle_count = subtitle_streams.len();
        let selected_subtitle_index = detail.selected_subtitle_index(subtitle_language);
        let media_source_select_open =
            detail.open_select == Some(SeriesDetailSelectKind::MediaSource) && source_count > 0;
        let subtitle_select_open =
            detail.open_select == Some(SeriesDetailSelectKind::Subtitle) && subtitle_count > 0;
        let can_play = !detail.playback_loading
            && !detail.video_sources_loading()
            && detail.selected_playback_item().is_some()
            && detail
                .selected_media_source()
                .and_then(|source| source.id.as_deref())
                .is_some_and(|id| !id.trim().is_empty());
        let play_label = detail_play_button_label(detail.playback_position_seconds());

        div().flex().flex_col().w_full().gap_2().child(
            div()
                .flex()
                .flex_wrap()
                .w_full()
                .items_center()
                .justify_between()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .min_w(px(150.0))
                        .h(px(42.0))
                        .justify_center()
                        .items_center()
                        .gap_2()
                        .rounded(px(8.0))
                        .id("series-detail-play-button")
                        .border_1()
                        .border_color(theme.accent)
                        .bg(theme.accent)
                        .px_4()
                        .text_base()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.accent_foreground)
                        .child(detail_play_button_icon(detail.playback_loading, theme))
                        .child(play_label)
                        .when(can_play, |this| {
                            this.cursor_pointer()
                                .hover(move |style| style.bg(theme.accent_hover))
                                .on_click(play)
                        })
                        .when(!can_play, |this| this.cursor_default().opacity(0.62)),
                )
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap_1()
                        .child(
                            detail_icon_button(
                                "series-detail-favorite-button",
                                if favorite {
                                    "icons/heart-filled.svg"
                                } else {
                                    "icons/heart.svg"
                                },
                                if favorite { "取消收藏" } else { "收藏" },
                                actions_enabled,
                                cx,
                            )
                            .when(actions_enabled, |this| this.on_click(toggle_favorite)),
                        )
                        .child(
                            detail_icon_button(
                                "series-detail-played-button",
                                if played {
                                    "icons/circle-check-filled.svg"
                                } else {
                                    "icons/circle-check.svg"
                                },
                                if played {
                                    "标记为未观看"
                                } else {
                                    "标记为已观看"
                                },
                                actions_enabled,
                                cx,
                            )
                            .when(actions_enabled, |this| {
                                this.on_click(cx.listener(|page, _, _, cx| {
                                    page.toggle_detail_played(false, cx)
                                }))
                            }),
                        )
                        .when(detail.is_series(), |this| {
                            this.child(
                                div()
                                    .relative()
                                    .child(
                                        detail_icon_button(
                                            "series-detail-more-button",
                                            "icons/ellipsis.svg",
                                            "剧集操作",
                                            true,
                                            cx,
                                        )
                                        .on_click(cx.listener(Self::toggle_detail_actions_menu)),
                                    )
                                    .when(
                                        detail.open_select == Some(SeriesDetailSelectKind::Actions),
                                        |this| {
                                            this.child(
                                                deferred(
                                                    self.render_series_actions_menu(detail, cx),
                                                )
                                                .with_priority(2),
                                            )
                                        },
                                    ),
                            )
                        }),
                )
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .flex_wrap()
                        .items_center()
                        .justify_end()
                        .gap_3()
                        .child(
                            div()
                                .relative()
                                .child(
                                    detail_select_box(
                                        "视频",
                                        video_label,
                                        source_count > 0 && !detail.video_sources_loading(),
                                        cx,
                                    )
                                    .id("series-detail-video-select")
                                    .on_click(toggle_video),
                                )
                                .when(media_source_select_open, |this| {
                                    this.child(
                                        deferred(detail_select_menu(
                                            "series-detail-video-menu",
                                            source_count,
                                            DETAIL_SELECT_WIDTH_PX,
                                            &detail.media_source_scroll_handle,
                                            cx,
                                            media_sources.iter().enumerate().map(
                                                |(index, source)| {
                                                    let label = source.name_label(index);
                                                    let selected =
                                                        selected_source_index == Some(index);
                                                    let on_click = cx.listener(
                                                        move |page: &mut HomeContent, _, _, cx| {
                                                            page.select_series_media_source(
                                                                index, cx,
                                                            );
                                                        },
                                                    );

                                                    detail_select_option(
                                                        label,
                                                        selected,
                                                        (
                                                            gpui::ElementId::from(
                                                                "series-detail-video-option",
                                                            ),
                                                            index.to_string(),
                                                        ),
                                                        cx,
                                                    )
                                                    .on_click(on_click)
                                                },
                                            ),
                                        ))
                                        .with_priority(1),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .relative()
                                .child(
                                    detail_select_box(
                                        "字幕",
                                        subtitle_label,
                                        subtitle_count > 0,
                                        cx,
                                    )
                                    .id("series-detail-subtitle-select")
                                    .on_click(toggle_subtitle),
                                )
                                .when(subtitle_select_open, |this| {
                                    this.child(
                                        deferred(detail_select_menu(
                                            "series-detail-subtitle-menu",
                                            subtitle_count,
                                            DETAIL_SELECT_WIDTH_PX,
                                            &detail.subtitle_scroll_handle,
                                            cx,
                                            subtitle_streams.iter().enumerate().map(
                                                |(index, stream)| {
                                                    let label = stream.display_title_label(index);
                                                    let selected =
                                                        selected_subtitle_index == Some(index);
                                                    let on_click = cx.listener(
                                                        move |page: &mut HomeContent, _, _, cx| {
                                                            page.select_series_subtitle(index, cx);
                                                        },
                                                    );

                                                    detail_select_option(
                                                        label,
                                                        selected,
                                                        (
                                                            gpui::ElementId::from(
                                                                "series-detail-subtitle-option",
                                                            ),
                                                            index.to_string(),
                                                        ),
                                                        cx,
                                                    )
                                                    .on_click(on_click)
                                                },
                                            ),
                                        ))
                                        .with_priority(1),
                                    )
                                }),
                        ),
                ),
        )
    }

    pub(super) fn render_series_detail_season_selector(
        &self,
        detail: &SeriesDetailState,
        seasons: &MediaItems,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let selected_season = detail.selected_season();
        let selected_id = selected_season.map(|season| season.id.as_str());
        let selected_label = selected_season
            .map(|season| season.name.clone())
            .unwrap_or_else(|| "请选择".to_string());
        let season_count = seasons.items.len();
        let select_width =
            season_select_width(seasons.items.iter().map(|season| season.name.as_str()));
        let menu_open =
            detail.open_select == Some(SeriesDetailSelectKind::Season) && season_count > 0;
        let toggle = cx.listener(Self::toggle_series_season_select);

        div()
            .relative()
            .w(px(select_width))
            .max_w_full()
            .child(
                season_popup_menu_trigger(selected_label, season_count > 0, select_width, cx)
                    .id("series-detail-season-select")
                    .on_click(toggle),
            )
            .when(menu_open, |this| {
                this.child(
                    deferred(detail_select_menu(
                        "series-detail-season-menu",
                        season_count,
                        select_width,
                        &detail.season_scroll_handle,
                        cx,
                        seasons.items.iter().enumerate().map(|(index, season)| {
                            let season_id = season.id.clone();
                            let selected = selected_id == Some(season_id.as_str());
                            let season_id_for_click = season_id.clone();
                            let on_click = cx.listener(move |page: &mut HomeContent, _, _, cx| {
                                page.select_series_season(season_id_for_click.clone(), cx);
                            });

                            detail_select_option(
                                season.name.clone(),
                                selected,
                                (
                                    gpui::ElementId::from("series-detail-season-option"),
                                    index.to_string(),
                                ),
                                cx,
                            )
                            .text_center()
                            .on_click(on_click)
                        }),
                    ))
                    .with_priority(1),
                )
            })
    }
}
