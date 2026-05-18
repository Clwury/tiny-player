use gpui::{
    Animation, AnimationExt as _, Context, InteractiveElement, IntoElement, MouseButton,
    ParentElement, StatefulInteractiveElement, Styled, StyledImage, Window, deferred, div,
    ease_in_out, img, prelude::FluentBuilder, px, svg,
};

use crate::{
    emby::{MediaItem, MediaItems},
    theme,
};

use super::super::{
    HomeContent,
    carousel::{
        DETAIL_EPISODE_CARD_GAP_PX, DETAIL_EPISODE_CARD_PADDING_PX, DETAIL_EPISODE_CARD_WIDTH_PX,
        HOME_MAIN_SCROLLBAR_WIDTH_PX, carousel_content_width_for,
        carousel_visible_range_between_for, home_main_content_width,
        max_carousel_scroll_offset_for,
    },
    components::{carousel_button, episode_card, format_community_rating},
};
use super::{SeriesDetailSelectKind, SeriesDetailState};

impl HomeContent {
    pub(crate) fn render_series_detail_scrollable_content(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let hero_height = (f32::from(window.bounds().size.height) * 0.6).max(260.0);
        let theme = theme::get(cx);
        let main_content_width = home_main_content_width(window);

        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .id("home-media-detail")
            .overflow_y_scroll()
            .scrollbar_width(px(HOME_MAIN_SCROLLBAR_WIDTH_PX))
            .track_scroll(&self.main_scroll_handle)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(Self::close_series_detail_select),
            )
            .child(div().flex().flex_col().w_full().when_some(
                self.series_detail.as_ref(),
                |this, detail| {
                    this.child(self.render_series_detail_hero(detail, hero_height, cx))
                        .child(
                            div()
                                .p_6()
                                .flex()
                                .flex_col()
                                .gap_5()
                                .when_some(detail.item_failed.clone(), |this, error| {
                                    this.child(div().text_sm().text_color(theme.error).child(error))
                                })
                                .when(
                                    detail.effects.item.is_loading() && detail.item.is_none(),
                                    |this| {
                                        this.child(
                                            div()
                                                .text_sm()
                                                .text_color(theme.muted_foreground)
                                                .child("加载媒体详情中…"),
                                        )
                                    },
                                )
                                .when(detail.item.is_some(), |this| {
                                    this.child(self.render_series_detail_controls(detail, cx))
                                })
                                .when(detail.is_series(), |this| {
                                    this.when_some(detail.next_up_failed.clone(), |this, error| {
                                        this.child(
                                            div().text_sm().text_color(theme.error).child(error),
                                        )
                                    })
                                    .when(
                                        detail.effects.next_up.is_loading()
                                            && detail.next_up.is_none(),
                                        |this| {
                                            this.child(
                                                div()
                                                    .text_sm()
                                                    .text_color(theme.muted_foreground)
                                                    .child("加载下一剧集中…"),
                                            )
                                        },
                                    )
                                    .when_some(detail.seasons_failed.clone(), |this, error| {
                                        this.child(
                                            div().text_sm().text_color(theme.error).child(error),
                                        )
                                    })
                                    .when(
                                        detail.effects.seasons.is_loading()
                                            && detail.seasons.is_none(),
                                        |this| {
                                            this.child(
                                                div()
                                                    .text_sm()
                                                    .text_color(theme.muted_foreground)
                                                    .child("加载季数中…"),
                                            )
                                        },
                                    )
                                    .when_some(detail.seasons.as_ref(), |this, seasons| {
                                        this.child(self.render_series_detail_season_selector(
                                            detail, seasons, cx,
                                        ))
                                    })
                                    .when_some(detail.episodes_failed.clone(), |this, error| {
                                        this.child(
                                            div().text_sm().text_color(theme.error).child(error),
                                        )
                                    })
                                    .when(
                                        detail.effects.episodes.is_loading()
                                            && detail.episodes.is_none(),
                                        |this| {
                                            this.child(
                                                div()
                                                    .text_sm()
                                                    .text_color(theme.muted_foreground)
                                                    .child("加载剧集列表中…"),
                                            )
                                        },
                                    )
                                    .when_some(detail.episodes.as_ref(), |this, episodes| {
                                        this.child(self.render_series_detail_episodes_row(
                                            detail,
                                            episodes,
                                            main_content_width,
                                            cx,
                                        ))
                                    })
                                    .when(
                                        !detail.effects.episodes.is_loading()
                                            && detail.episodes.is_none(),
                                        |this| {
                                            this.child(
                                                div()
                                                    .text_sm()
                                                    .text_color(theme.muted_foreground)
                                                    .child("暂无剧集"),
                                            )
                                        },
                                    )
                                }),
                        )
                },
            ))
    }

    pub(crate) fn render_series_detail_back_button(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);
        let close_detail = cx.listener(Self::close_series_detail);

        div()
            .id("series-detail-back-button")
            .absolute()
            .left_4()
            .top_4()
            .flex()
            .size(px(32.0))
            .items_center()
            .justify_center()
            .rounded_md()
            .hover(move |style| style.bg(theme.secondary_hover))
            .child(
                svg()
                    .path("icons/chevron-left.svg")
                    .size(px(18.0))
                    .text_color(theme.foreground),
            )
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(close_detail)
    }

    fn render_series_detail_hero(
        &self,
        detail: &SeriesDetailState,
        hero_height: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let backdrop_path = detail
            .item
            .as_ref()
            .and_then(|item| self.image_path_for_series_backdrop(item));
        let logo_path = detail
            .item
            .as_ref()
            .and_then(|item| self.image_path_for_series_logo(item));
        let display_title = detail
            .item
            .as_ref()
            .map(|item| item.name.clone())
            .unwrap_or_else(|| detail.title.clone());
        let episode_line = detail.hero_line();

        div()
            .relative()
            .w_full()
            .h(px(hero_height))
            .overflow_hidden()
            .bg(theme.input_background)
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .left_0()
                    .when_some(backdrop_path, |this, path| {
                        this.child(
                            img(path)
                                .w_full()
                                .h_full()
                                .object_fit(gpui::ObjectFit::Cover),
                        )
                    }),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .bottom_0()
                    .left_0()
                    .bg(theme.background.opacity(0.35)),
            )
            .child(
                div()
                    .absolute()
                    .left_6()
                    .right_6()
                    .bottom_6()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .text_color(theme.foreground)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .when_some(logo_path.clone(), |this, path| {
                                this.child(img(path).w(px(200.0)))
                            })
                            .when(logo_path.is_none(), |this| {
                                this.child(
                                    div()
                                        .max_w(px(760.0))
                                        .truncate()
                                        .text_lg()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child(display_title),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .h(px(24.0))
                            .max_w(px(760.0))
                            .items_center()
                            .truncate()
                            .text_base()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .when_some(episode_line, |this, line| this.child(line)),
                    )
                    .when_some(detail.item.as_ref(), |this, item| {
                        this.child(self.render_series_detail_metadata_row(item, cx))
                    }),
            )
    }

    fn render_series_detail_metadata_row(
        &self,
        item: &MediaItem,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let official_rating = item
            .official_rating
            .as_deref()
            .map(str::trim)
            .filter(|rating| !rating.is_empty())
            .map(ToString::to_string);
        let genres = item
            .genres
            .as_ref()
            .filter(|genres| !genres.is_empty())
            .map(|genres| genres.join(", "));

        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .text_sm()
            .when_some(item.community_rating, |this, rating| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .rounded_full()
                        .bg(theme.dialog_background.opacity(0.86))
                        .px_3()
                        .py_1()
                        .text_color(theme.foreground)
                        .child(
                            svg()
                                .path("icons/star.svg")
                                .size(px(14.0))
                                .text_color(theme.warning),
                        )
                        .child(format_community_rating(rating)),
                )
            })
            .when_some(official_rating, |this, rating| {
                this.child(
                    div()
                        .rounded_full()
                        .border_1()
                        .border_color(theme.input_border)
                        .bg(theme.dialog_background.opacity(0.86))
                        .px_3()
                        .py_1()
                        .text_color(theme.foreground)
                        .child(rating),
                )
            })
            .when_some(genres, |this, genres| {
                this.child(
                    div()
                        .rounded_full()
                        .bg(theme.dialog_background.opacity(0.86))
                        .px_3()
                        .py_1()
                        .text_color(theme.foreground)
                        .child(genres),
                )
            })
    }

    fn render_series_detail_controls(
        &self,
        detail: &SeriesDetailState,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let video_label = detail.selected_media_source_label();
        let subtitle_label = detail.selected_subtitle_label();
        let play = cx.listener(Self::play_selected_media);
        let toggle_video = cx.listener(Self::toggle_series_media_source_select);
        let toggle_subtitle = cx.listener(Self::toggle_series_subtitle_select);
        let media_sources = detail
            .selected_playback_item()
            .and_then(|item| item.media_sources.as_deref())
            .unwrap_or_default();
        let source_count = media_sources.len();
        let selected_source_index = detail.selected_media_source_index();
        let subtitle_streams = detail
            .selected_media_source()
            .map(|source| source.subtitle_streams())
            .unwrap_or_default();
        let subtitle_count = subtitle_streams.len();
        let selected_subtitle_index = detail.selected_subtitle_index();
        let media_source_select_open =
            detail.open_select == Some(SeriesDetailSelectKind::MediaSource) && source_count > 0;
        let subtitle_select_open =
            detail.open_select == Some(SeriesDetailSelectKind::Subtitle) && subtitle_count > 0;
        let can_play = !detail.playback_loading
            && detail.selected_playback_item().is_some()
            && detail
                .selected_media_source()
                .and_then(|source| source.id.as_deref())
                .is_some_and(|id| !id.trim().is_empty());
        let play_label = if detail.playback_loading {
            "获取播放地址中…"
        } else {
            "播放"
        };

        div()
            .flex()
            .flex_col()
            .w_full()
            .gap_2()
            .child(
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
                            .w(px(150.0))
                            .h(px(42.0))
                            .justify_center()
                            .items_center()
                            .gap_2()
                            .rounded(px(8.0))
                            .id("series-detail-play-button")
                            .border_1()
                            .border_color(theme.input_border_focused)
                            .bg(theme.foreground)
                            .px_4()
                            .text_base()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.background)
                            .child(
                                svg()
                                    .path("icons/play.svg")
                                    .size(px(18.0))
                                    .text_color(theme.background),
                            )
                            .child(play_label)
                            .when(can_play, |this| this.cursor_pointer().on_click(play))
                            .when(!can_play, |this| this.cursor_default().opacity(0.62)),
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
                                        detail_select_box("视频", video_label, source_count > 0, cx)
                                            .id("series-detail-video-select")
                                            .on_click(toggle_video),
                                    )
                                    .when(media_source_select_open, |this| {
                                        this.child(
                                            deferred(detail_select_menu(
                                                "series-detail-video-menu",
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

                                                        detail_select_option(label, selected, cx)
                                                            .id((
                                                                gpui::ElementId::from(
                                                                    "series-detail-video-option",
                                                                ),
                                                                index.to_string(),
                                                            ))
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
                                        detail_select_box("字幕", subtitle_label, subtitle_count > 0, cx)
                                            .id("series-detail-subtitle-select")
                                            .on_click(toggle_subtitle),
                                    )
                                    .when(subtitle_select_open, |this| {
                                        this.child(
                                            deferred(detail_select_menu(
                                                "series-detail-subtitle-menu",
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

                                                        detail_select_option(label, selected, cx)
                                                            .id((
                                                                gpui::ElementId::from(
                                                                    "series-detail-subtitle-option",
                                                                ),
                                                                index.to_string(),
                                                            ))
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
            .when_some(detail.playback_failed.clone(), |this, error| {
                this.child(div().text_sm().text_color(theme.error).child(error))
            })
    }

    fn render_series_detail_season_selector(
        &self,
        detail: &SeriesDetailState,
        seasons: &MediaItems,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let selected_id = detail.selected_season().map(|season| season.id.as_str());

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(seasons.items.iter().map(|season| {
                        let season_id = season.id.clone();
                        let selected = selected_id == Some(season_id.as_str());
                        let season_id_for_click = season_id.clone();
                        let on_click = cx.listener(move |page: &mut HomeContent, _, _, cx| {
                            page.select_series_season(season_id_for_click.clone(), cx);
                        });

                        season_pill(season.name.clone(), selected, cx)
                            .id((gpui::ElementId::from("series-detail-season"), season_id))
                            .on_click(on_click)
                    })),
            )
    }

    fn render_series_detail_episodes_row(
        &self,
        detail: &SeriesDetailState,
        episodes: &MediaItems,
        viewport_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let viewport_width = viewport_width.min(carousel_content_width_for(
            episodes.items.len(),
            DETAIL_EPISODE_CARD_WIDTH_PX,
            DETAIL_EPISODE_CARD_PADDING_PX,
            DETAIL_EPISODE_CARD_GAP_PX,
        ));
        let max_offset = max_carousel_scroll_offset_for(
            episodes.items.len(),
            viewport_width,
            DETAIL_EPISODE_CARD_WIDTH_PX,
            DETAIL_EPISODE_CARD_PADDING_PX,
            DETAIL_EPISODE_CARD_GAP_PX,
        );
        let carousel = detail.episodes_carousel;
        let offset = carousel.scroll_offset(max_offset);
        let previous_offset = carousel.previous_scroll_offset(max_offset);
        let visible_range = carousel_visible_range_between_for(
            episodes.items.len(),
            (previous_offset, offset),
            viewport_width,
            DETAIL_EPISODE_CARD_WIDTH_PX,
            DETAIL_EPISODE_CARD_PADDING_PX,
            DETAIL_EPISODE_CARD_GAP_PX,
            (2, 3),
        );
        let animation_id = carousel.animation_id();
        let has_controls = max_offset > 0.0;
        let controls_visible = carousel.controls_visible(has_controls);
        let on_hover = cx.listener(|page: &mut HomeContent, hovered: &bool, _, cx| {
            page.set_series_episodes_hovered(*hovered, cx);
        });
        let left_controls_hover = cx.listener(|page: &mut HomeContent, hovered: &bool, _, cx| {
            page.set_series_episodes_controls_hovered(*hovered, cx);
        });
        let right_controls_hover = cx.listener(|page: &mut HomeContent, hovered: &bool, _, cx| {
            page.set_series_episodes_controls_hovered(*hovered, cx);
        });
        let scroll_left = cx.listener(Self::scroll_series_episodes_left);
        let scroll_right = cx.listener(Self::scroll_series_episodes_right);
        let selected_episode_id = detail.selected_episode_id.as_deref();
        let animation_key = gpui::ElementId::from((
            gpui::ElementId::from("series-detail-episodes-scroll"),
            format!("{}-{animation_id}", detail.series_id),
        ));

        div().flex().flex_col().gap_3().child(
            div()
                .id("series-detail-episodes-row")
                .relative()
                .group("series-detail-episodes-row")
                .w(px(viewport_width))
                .max_w_full()
                .overflow_hidden()
                .on_hover(on_hover)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_4()
                        .when(visible_range.leading_width > 0.0, |this| {
                            this.child(div().flex_none().w(px(visible_range.leading_width)))
                        })
                        .children(
                            episodes.items[visible_range.start..visible_range.end]
                                .iter()
                                .map(|episode| {
                                    let episode_id = episode.id.clone();
                                    let selected = selected_episode_id == Some(episode_id.as_str());
                                    let episode_id_for_click = episode_id.clone();
                                    let on_click =
                                        cx.listener(move |page: &mut HomeContent, _, _, cx| {
                                            page.select_series_episode(
                                                episode_id_for_click.clone(),
                                                cx,
                                            );
                                        });
                                    let image_path = self.image_path_for_episode_primary(episode);

                                    episode_card(episode, image_path, selected, cx)
                                        .id((
                                            gpui::ElementId::from("series-detail-episode-card"),
                                            episode_id,
                                        ))
                                        .on_click(on_click)
                                }),
                        )
                        .when(visible_range.trailing_width > 0.0, |this| {
                            this.child(div().flex_none().w(px(visible_range.trailing_width)))
                        })
                        .with_animation(
                            animation_key,
                            Animation::new(std::time::Duration::from_millis(220))
                                .with_easing(ease_in_out),
                            move |track, delta| {
                                track
                                    .ml(px(-(previous_offset + (offset - previous_offset) * delta)))
                            },
                        ),
                )
                .when(has_controls, |this| {
                    this.child(carousel_button(
                        "series-detail-episodes-scroll-left",
                        "icons/chevron-left.svg",
                        false,
                        controls_visible,
                        theme,
                        left_controls_hover,
                        scroll_left,
                    ))
                })
                .when(has_controls, |this| {
                    this.child(carousel_button(
                        "series-detail-episodes-scroll-right",
                        "icons/chevron-right.svg",
                        true,
                        controls_visible,
                        theme,
                        right_controls_hover,
                        scroll_right,
                    ))
                }),
        )
    }
}

fn detail_select_box<T>(
    label: &'static str,
    value: String,
    enabled: bool,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(34.0))
        .w(px(250.0))
        .max_w_full()
        .items_center()
        .justify_between()
        .gap_2()
        .rounded(px(8.0))
        .border_1()
        .border_color(if enabled {
            theme.input_border
        } else {
            theme.input_border.opacity(0.62)
        })
        .bg(theme.dialog_background.opacity(0.88))
        .px_3()
        .text_sm()
        .text_color(if enabled {
            theme.foreground
        } else {
            theme.muted_foreground
        })
        .when(enabled, |this| {
            this.hover(move |style| style.bg(theme.secondary_hover))
        })
        .when(!enabled, |this| this.opacity(0.62))
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(
            div()
                .flex()
                .min_w_0()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child(value),
                ),
        )
        .child(
            svg()
                .path("icons/chevron-right.svg")
                .size(px(14.0))
                .text_color(theme.muted_foreground),
        )
}

fn detail_select_menu<T, I, E>(id: &'static str, cx: &Context<T>, children: I) -> impl IntoElement
where
    I: IntoIterator<Item = E>,
    E: IntoElement,
{
    let theme = theme::get(cx);

    div()
        .id(id)
        .absolute()
        .top(px(40.0))
        .left_0()
        .flex()
        .flex_col()
        .w(px(250.0))
        .max_h(px(220.0))
        .overflow_y_scroll()
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.input_border_focused)
        .bg(theme.dialog_background)
        .shadow_lg()
        .p(px(6.0))
        .gap_1()
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .children(children)
}

fn detail_select_option<T>(label: String, selected: bool, cx: &Context<T>) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .min_h(px(30.0))
        .items_center()
        .rounded(px(6.0))
        .px_2()
        .py_1()
        .text_sm()
        .font_weight(if selected {
            gpui::FontWeight::SEMIBOLD
        } else {
            gpui::FontWeight::NORMAL
        })
        .text_color(if selected {
            theme.foreground
        } else {
            theme.muted_foreground
        })
        .bg(if selected {
            theme.secondary_hover
        } else {
            theme.dialog_background
        })
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(div().flex_1().min_w_0().text_ellipsis().child(label))
}

fn season_pill<T>(label: String, selected: bool, cx: &Context<T>) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(32.0))
        .items_center()
        .rounded_full()
        .border_1()
        .border_color(if selected {
            theme.input_border_focused
        } else {
            theme.input_border
        })
        .bg(if selected {
            theme.secondary_hover
        } else {
            theme.dialog_background.opacity(0.86)
        })
        .px_4()
        .text_sm()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(if selected {
            theme.foreground
        } else {
            theme.muted_foreground
        })
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(label)
}
