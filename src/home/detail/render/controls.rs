use super::*;
use crate::home::detail::binding::detail_binding;
use crate::home::track_preferences::detail_track_choices;
use crate::media::{PlaybackLanguagePreferences, track_metadata_label};
use crate::ui::radius;

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
        detail: DetailView<'_>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let colors = &theme::get(cx).context_menu;
        let actions = self.controller.detail_actions(detail.model, true);
        let favorite = actions.favorite;
        let played = actions.played;
        let enabled = actions.enabled;
        let foreground = if enabled {
            colors.foreground
        } else {
            colors.disabled_foreground
        };
        div()
            .id("series-detail-actions-menu")
            .cursor_default()
            .debug_selector(|| "series-detail-actions-menu".into())
            .absolute()
            .top(px(34.0))
            .left_0()
            .w(px(164.0))
            .p_1()
            .flex()
            .flex_col()
            .gap_1()
            .rounded(radius::SURFACE)
            .border_1()
            .border_color(colors.border)
            .bg(colors.background)
            .shadow_lg()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .when_some(
                detail.presentation.action_menu_focus.as_ref(),
                |this, focus| this.track_focus(focus),
            )
            .on_key_down(cx.listener(|page, event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    if let Some(detail) =
                        detail_binding(page.controller.detail_view(), &mut page.detail_resources)
                    {
                        detail.presentation.open_select = None;
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
                        .rounded(radius::CONTROL)
                        .text_sm()
                        .text_color(foreground)
                        .child(
                            svg()
                                .path(icon)
                                .size(px(16.0))
                                .flex_none()
                                .text_color(foreground),
                        )
                        .child(label)
                        .when(enabled, |this| {
                            this.cursor_pointer()
                                .hover(move |style| style.bg(colors.hover_background))
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
                        .when(!enabled, |this| this.cursor_default())
                }),
            )
    }

    pub(super) fn render_series_detail_controls(
        &self,
        detail: DetailView<'_>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let saved_tracks = detail_track_choices(detail.model, &self.current_server, cx);
        let view = self.controller.detail_controls(
            detail.model,
            PlaybackLanguagePreferences::get(cx).subtitle,
            &saved_tracks,
        );
        let play = cx.listener(Self::play_selected_media);
        let toggle_video = cx.listener(Self::toggle_series_media_source_select);
        let toggle_subtitle = cx.listener(Self::toggle_series_subtitle_select);
        let toggle_favorite = cx.listener(Self::toggle_detail_favorite);
        let favorite = view.actions.favorite;
        let played = view.actions.played;
        let actions_enabled = view.actions.enabled;
        let media_sources = view.media_sources;
        let source_count = media_sources.len();
        let subtitle_streams = view.subtitle_streams;
        let subtitle_count = subtitle_streams.len();
        let selected_source_index = view.selected_source_index;
        let selected_subtitle_index = view.selected_subtitle_index;
        let subtitle_select_enabled = view.subtitle_select_enabled;
        let media_source_select_open = detail.presentation.open_select
            == Some(SeriesDetailSelectKind::MediaSource)
            && source_count > 0;
        let subtitle_select_open = detail.presentation.open_select
            == Some(SeriesDetailSelectKind::Subtitle)
            && subtitle_select_enabled;
        let can_play = view.can_play;
        let play_label = detail_play_button_label(view.playback_position_seconds);

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
                        .rounded(radius::CONTROL)
                        .id("series-detail-play-button")
                        .border_1()
                        .border_color(theme.accent)
                        .bg(theme.accent)
                        .px_4()
                        .text_base()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.accent_foreground)
                        .child(detail_play_button_icon(
                            detail.model.playback_loading,
                            theme,
                        ))
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
                        .when(view.is_series, |this| {
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
                                        detail.presentation.open_select
                                            == Some(SeriesDetailSelectKind::Actions),
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
                                        view.video_label,
                                        view.media_source_select_enabled,
                                        cx,
                                    )
                                    .id("series-detail-video-select")
                                    .debug_selector(|| "series-detail-video-select".into())
                                    .on_click(toggle_video),
                                )
                                .when(media_source_select_open, |this| {
                                    this.child(
                                        deferred(detail_select_menu(
                                            "series-detail-video-menu",
                                            source_count,
                                            DETAIL_SELECT_WIDTH_PX,
                                            DETAIL_TWO_LINE_OPTION_HEIGHT_PX,
                                            &detail.presentation.media_source_scroll_handle,
                                            cx,
                                            media_sources.iter().enumerate().map(
                                                |(index, source)| {
                                                    let label = source.name_label(index);
                                                    let subtitle =
                                                        video_metadata::video_metadata_label(
                                                            source,
                                                        );
                                                    let selected =
                                                        selected_source_index == Some(index);
                                                    let on_click = cx.listener(
                                                        move |page: &mut HomeContent, _, _, cx| {
                                                            page.select_series_media_source(
                                                                index, cx,
                                                            );
                                                        },
                                                    );

                                                    detail_select_option_with_subtitle(
                                                        label,
                                                        subtitle.as_deref(),
                                                        selected,
                                                        format!(
                                                            "series-detail-video-option-{index}"
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
                                        view.subtitle_label,
                                        subtitle_select_enabled,
                                        cx,
                                    )
                                    .id("series-detail-subtitle-select")
                                    .debug_selector(|| "series-detail-subtitle-select".into())
                                    .on_click(toggle_subtitle),
                                )
                                .when(subtitle_select_open, |this| {
                                    this.child(
                                        deferred(detail_select_menu(
                                            "series-detail-subtitle-menu",
                                            subtitle_count + 1,
                                            DETAIL_SELECT_WIDTH_PX,
                                            DETAIL_TWO_LINE_OPTION_HEIGHT_PX,
                                            &detail.presentation.subtitle_scroll_handle,
                                            cx,
                                            std::iter::once(
                                                detail_select_option_with_subtitle(
                                                    "Off".into(),
                                                    Some("off"),
                                                    selected_subtitle_index.is_none(),
                                                    "series-detail-subtitle-off-option".into(),
                                                    cx,
                                                )
                                                .on_click(cx.listener(
                                                    |page: &mut HomeContent, _, _, cx| {
                                                        page.select_series_subtitle(None, cx);
                                                    },
                                                )),
                                            )
                                            .chain(
                                                subtitle_streams.iter().enumerate().map(
                                                    |(index, stream)| {
                                                        let label =
                                                            stream.display_title_label(index);
                                                        let subtitle = track_metadata_label(
                                                            stream.language.as_deref(),
                                                            stream.title.as_deref(),
                                                        );
                                                        let selected =
                                                            selected_subtitle_index == Some(index);
                                                        let on_click = cx.listener(
                                                        move |page: &mut HomeContent, _, _, cx| {
                                                            page.select_series_subtitle(
                                                                Some(index),
                                                                cx,
                                                            );
                                                        },
                                                    );

                                                        detail_select_option_with_subtitle(
                                                        label,
                                                        Some(&subtitle),
                                                        selected,
                                                        format!(
                                                            "series-detail-subtitle-option-{index}"
                                                        ),
                                                        cx,
                                                    )
                                                    .on_click(on_click)
                                                    },
                                                ),
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
        detail: DetailView<'_>,
        seasons: &MediaItems,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let selected_season = detail.model.selected_season();
        let selected_id = selected_season.map(|season| season.id.as_str());
        let selected_label = selected_season
            .map(|season| season.name.clone())
            .unwrap_or_else(|| "请选择".to_string());
        let season_count = seasons.items.len();
        let select_width = season_select_width(
            seasons.items.iter().map(|season| season.name.as_str()),
            window,
        );
        let menu_open = detail.presentation.open_select == Some(SeriesDetailSelectKind::Season)
            && season_count > 0;
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
                        DETAIL_SELECT_OPTION_HEIGHT_PX,
                        &detail.presentation.season_scroll_handle,
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
                            .px(px(4.0))
                            .text_center()
                            .on_click(on_click)
                        }),
                    ))
                    .with_priority(1),
                )
            })
    }
}
