use gpui::{
    Animation, AnimationExt as _, App, ClickEvent, Context, InteractiveElement, IntoElement,
    MouseButton, ParentElement, ScrollHandle, StatefulInteractiveElement, Styled, StyledImage,
    Window, deferred, div, ease_in_out, img, prelude::FluentBuilder, px, svg,
};

use crate::{
    emby::{MediaItem, MediaItems, MediaPerson, UserItems},
    theme,
    ui::{
        scrollbar::{SCROLLBAR_WIDTH_PX, Scrollbar},
        tooltip::text_tooltip,
    },
};

use super::super::{
    HomeContent,
    carousel::{
        DETAIL_EPISODE_CARD_GAP_PX, DETAIL_EPISODE_CARD_PADDING_PX, DETAIL_EPISODE_CARD_WIDTH_PX,
        DETAIL_PERSON_CARD_GAP_PX, DETAIL_PERSON_CARD_PADDING_PX, DETAIL_PERSON_CARD_WIDTH_PX,
        HOME_ITEM_CARD_GAP_PX, HOME_ITEM_CARD_PADDING_PX, HOME_ITEM_CARD_WIDTH_PX,
        HOME_MAIN_SCROLLBAR_WIDTH_PX, carousel_content_width_for,
        carousel_visible_range_between_for, home_main_content_width,
        max_carousel_scroll_offset_for,
    },
    components::{
        carousel_button, episode_card, format_community_rating, home_section_title, person_card,
        user_item_card,
    },
};
use super::{SeriesDetailSelectKind, SeriesDetailState};

const DETAIL_SELECT_MAX_VISIBLE_OPTIONS: usize = 5;
const DETAIL_SELECT_OPTION_HEIGHT_PX: f32 = 28.0;
const DETAIL_SELECT_MENU_MAX_HEIGHT_PX: f32 =
    DETAIL_SELECT_OPTION_HEIGHT_PX * 5.0 + 4.0 * 4.0 + 6.0 * 2.0;
const DETAIL_SELECT_WIDTH_PX: f32 = 250.0;
const DETAIL_SELECT_TOOLTIP_WIDTH_UNITS: usize = 30;
const SEASON_SELECT_MIN_WIDTH_PX: f32 = 100.0;
const SEASON_SELECT_MAX_WIDTH_PX: f32 = 320.0;
const SEASON_SELECT_HORIZONTAL_PADDING_PX: f32 = 32.0;
const SELECT_TEXT_UNIT_WIDTH_PX: f32 = 7.0;

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
            .track_scroll(
                &self
                    .series_detail
                    .as_ref()
                    .expect("detail route has detail state")
                    .scroll_handle,
            )
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
                                    this.child(detail_error_with_retry(
                                        "retry-detail-item",
                                        error,
                                        cx.listener(|page, _, _, cx| {
                                            page.load_series_media_item_if_needed(cx)
                                        }),
                                        cx,
                                    ))
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
                                        this.child(detail_error_with_retry(
                                            "retry-detail-next-up",
                                            error,
                                            cx.listener(|page, _, _, cx| {
                                                page.load_series_next_up_if_needed(cx)
                                            }),
                                            cx,
                                        ))
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
                                        this.child(detail_error_with_retry(
                                            "retry-detail-seasons",
                                            error,
                                            cx.listener(|page, _, _, cx| {
                                                page.load_series_seasons_if_needed(cx)
                                            }),
                                            cx,
                                        ))
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
                                        this.child(detail_error_with_retry(
                                            "retry-detail-episodes",
                                            error,
                                            cx.listener(|page, _, _, cx| {
                                                page.load_series_episodes_if_needed(cx)
                                            }),
                                            cx,
                                        ))
                                    })
                                    .when_some(
                                        detail.episode_selection_warning.clone(),
                                        |this, warning| {
                                            this.child(
                                                div()
                                                    .text_sm()
                                                    .text_color(theme.muted_foreground)
                                                    .child(warning),
                                            )
                                        },
                                    )
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
                                })
                                .when_some(
                                    detail
                                        .item
                                        .as_ref()
                                        .and_then(|item| item.people.as_deref())
                                        .filter(|people| !people.is_empty()),
                                    |this, people| {
                                        this.child(self.render_series_detail_people_row(
                                            detail,
                                            people,
                                            main_content_width,
                                            cx,
                                        ))
                                    },
                                )
                                .child(self.render_series_detail_similar_section(
                                    detail,
                                    main_content_width,
                                    cx,
                                ))
                                .when_some(detail.item.as_ref(), |this, item| {
                                    this.child(self.render_series_detail_studios_row(item, cx))
                                        .child(self.render_series_detail_links_row(item, cx))
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
            .occlude()
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
        let toggle_favorite = cx.listener(Self::toggle_detail_favorite);
        let favorite = self
            .effective_user_data(
                &detail.series_id,
                detail
                    .item
                    .as_ref()
                    .and_then(|item| item.user_data.as_ref()),
            )
            .is_some_and(|data| data.is_favorite);
        let favorite_pending = self.favorite_is_pending(&detail.series_id);
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
        let play_label =
            detail_play_button_label(detail.playback_loading, detail.playback_position_seconds());

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
                            .id("series-detail-favorite-button")
                            .flex()
                            .size(px(32.0))
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(theme.input_border)
                            .bg(theme.input_background)
                            .child(
                                svg()
                                    .path(if favorite {
                                        "icons/heart.svg"
                                    } else {
                                        "icons/heart-off.svg"
                                    })
                                    .size(px(14.0))
                                    .text_color(if favorite {
                                        theme.error
                                    } else {
                                        theme.foreground
                                    }),
                            )
                            .when(!favorite_pending, |this| {
                                this.cursor_pointer()
                                    .hover(move |style| style.bg(theme.secondary_hover))
                                    .on_click(toggle_favorite)
                            })
                            .when(favorite_pending, |this| {
                                this.cursor_default().opacity(0.55)
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
                                        detail_select_box("视频", video_label, source_count > 0, cx)
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
                                        detail_select_box("字幕", subtitle_label, subtitle_count > 0, cx)
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
            .when_some(detail.playback_failed.clone(), |this, error| {
                this.child(div().text_sm().text_color(theme.error).child(error))
            })
            .when_some(self.detail_favorite_error(), |this, error| {
                this.child(div().text_sm().text_color(theme.error).child(error))
            })
    }

    fn render_series_detail_season_selector(
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

    fn render_series_detail_people_row(
        &self,
        detail: &SeriesDetailState,
        people: &[MediaPerson],
        viewport_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let viewport_width = viewport_width.min(carousel_content_width_for(
            people.len(),
            DETAIL_PERSON_CARD_WIDTH_PX,
            DETAIL_PERSON_CARD_PADDING_PX,
            DETAIL_PERSON_CARD_GAP_PX,
        ));
        let max_offset = max_carousel_scroll_offset_for(
            people.len(),
            viewport_width,
            DETAIL_PERSON_CARD_WIDTH_PX,
            DETAIL_PERSON_CARD_PADDING_PX,
            DETAIL_PERSON_CARD_GAP_PX,
        );
        let carousel = detail.people_carousel;
        let offset = carousel.scroll_offset(max_offset);
        let previous_offset = carousel.previous_scroll_offset(max_offset);
        let visible_range = carousel_visible_range_between_for(
            people.len(),
            (previous_offset, offset),
            viewport_width,
            DETAIL_PERSON_CARD_WIDTH_PX,
            DETAIL_PERSON_CARD_PADDING_PX,
            DETAIL_PERSON_CARD_GAP_PX,
            (3, 5),
        );
        let animation_id = carousel.animation_id();
        let has_controls = max_offset > 0.0;
        let controls_visible = carousel.controls_visible(has_controls);
        let on_hover = cx.listener(|page: &mut HomeContent, hovered: &bool, _, cx| {
            page.set_series_people_hovered(*hovered, cx);
        });
        let left_controls_hover = cx.listener(|page: &mut HomeContent, hovered: &bool, _, cx| {
            page.set_series_people_controls_hovered(*hovered, cx);
        });
        let right_controls_hover = cx.listener(|page: &mut HomeContent, hovered: &bool, _, cx| {
            page.set_series_people_controls_hovered(*hovered, cx);
        });
        let scroll_left = cx.listener(Self::scroll_series_people_left);
        let scroll_right = cx.listener(Self::scroll_series_people_right);
        let animation_key = gpui::ElementId::from((
            gpui::ElementId::from("series-detail-people-scroll"),
            format!("{}-{animation_id}", detail.series_id),
        ));

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_lg()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child("演职人员"),
            )
            .child(
                div()
                    .id("series-detail-people-row")
                    .relative()
                    .group("series-detail-people-row")
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
                                people[visible_range.start..visible_range.end]
                                    .iter()
                                    .enumerate()
                                    .map(|(local_index, person)| {
                                        let index = visible_range.start + local_index;
                                        let person_key =
                                            person.id().unwrap_or("unknown").to_string();
                                        let image_path = self.image_path_for_person_primary(person);

                                        person_card(person, image_path, cx).id((
                                            gpui::ElementId::from("series-detail-person-card"),
                                            format!("{person_key}-{index}"),
                                        ))
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
                                    track.ml(px(
                                        -(previous_offset + (offset - previous_offset) * delta)
                                    ))
                                },
                            ),
                    )
                    .when(has_controls, |this| {
                        this.child(carousel_button(
                            "series-detail-people-scroll-left",
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
                            "series-detail-people-scroll-right",
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

    fn render_series_detail_similar_section(
        &self,
        detail: &SeriesDetailState,
        viewport_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let items = detail.similar_items.as_ref();
        let show_empty = !detail.effects.similar.is_loading()
            && detail.similar_failed.is_none()
            && items.is_none_or(|items| items.items.is_empty());

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(home_section_title("相似作品", cx))
            .when_some(detail.similar_failed.clone(), |this, error| {
                this.child(detail_error_with_retry(
                    "retry-detail-similar",
                    error,
                    cx.listener(|page, _, _, cx| page.load_similar_items_if_needed(cx)),
                    cx,
                ))
            })
            .when(
                detail.effects.similar.is_loading() && items.is_none(),
                |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("加载相似作品中…"),
                    )
                },
            )
            .when_some(
                items.filter(|items| !items.items.is_empty()),
                |this, items| {
                    this.child(self.render_series_detail_similar_row(
                        detail,
                        items,
                        viewport_width,
                        cx,
                    ))
                },
            )
            .when(show_empty, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("暂无相似作品"),
                )
            })
    }

    fn render_series_detail_similar_row(
        &self,
        detail: &SeriesDetailState,
        items: &UserItems,
        viewport_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let viewport_width = viewport_width.min(carousel_content_width_for(
            items.items.len(),
            HOME_ITEM_CARD_WIDTH_PX,
            HOME_ITEM_CARD_PADDING_PX,
            HOME_ITEM_CARD_GAP_PX,
        ));
        let max_offset = max_carousel_scroll_offset_for(
            items.items.len(),
            viewport_width,
            HOME_ITEM_CARD_WIDTH_PX,
            HOME_ITEM_CARD_PADDING_PX,
            HOME_ITEM_CARD_GAP_PX,
        );
        let carousel = detail.similar_carousel;
        let offset = carousel.scroll_offset(max_offset);
        let previous_offset = carousel.previous_scroll_offset(max_offset);
        let visible_range = carousel_visible_range_between_for(
            items.items.len(),
            (previous_offset, offset),
            viewport_width,
            HOME_ITEM_CARD_WIDTH_PX,
            HOME_ITEM_CARD_PADDING_PX,
            HOME_ITEM_CARD_GAP_PX,
            (2, 4),
        );
        let animation_id = carousel.animation_id();
        let has_controls = max_offset > 0.0;
        let controls_visible = carousel.controls_visible(has_controls);
        let on_hover = cx.listener(|page: &mut HomeContent, hovered: &bool, _, cx| {
            page.set_series_similar_hovered(*hovered, cx);
        });
        let left_controls_hover = cx.listener(|page: &mut HomeContent, hovered: &bool, _, cx| {
            page.set_series_similar_controls_hovered(*hovered, cx);
        });
        let right_controls_hover = cx.listener(|page: &mut HomeContent, hovered: &bool, _, cx| {
            page.set_series_similar_controls_hovered(*hovered, cx);
        });
        let scroll_left = cx.listener(Self::scroll_series_similar_left);
        let scroll_right = cx.listener(Self::scroll_series_similar_right);
        let animation_key = gpui::ElementId::from((
            gpui::ElementId::from("series-detail-similar-scroll"),
            format!("{}-{animation_id}", detail.series_id),
        ));

        div()
            .id("series-detail-similar-row")
            .relative()
            .group("series-detail-similar-row")
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
                        items.items[visible_range.start..visible_range.end]
                            .iter()
                            .map(|item| {
                                let item = self.effective_user_item(item);
                                let image_path = self.image_path_for_user_item(&item);
                                let item_id = item.id.clone();
                                let card = user_item_card(&item, image_path, cx).id((
                                    gpui::ElementId::from("series-detail-similar-card"),
                                    item_id,
                                ));

                                if matches!(item.item_type.as_deref(), Some("Series" | "Movie")) {
                                    let item = item.clone();
                                    let on_click =
                                        cx.listener(move |page: &mut HomeContent, _, _, cx| {
                                            page.open_media_detail(&item, cx);
                                        });
                                    card.cursor_pointer().on_click(on_click)
                                } else {
                                    card
                                }
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
                            track.ml(px(-(previous_offset + (offset - previous_offset) * delta)))
                        },
                    ),
            )
            .when(has_controls, |this| {
                this.child(carousel_button(
                    "series-detail-similar-scroll-left",
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
                    "series-detail-similar-scroll-right",
                    "icons/chevron-right.svg",
                    true,
                    controls_visible,
                    theme,
                    right_controls_hover,
                    scroll_right,
                ))
            })
    }

    fn render_series_detail_studios_row(
        &self,
        item: &MediaItem,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let studios = item
            .studios
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|studio| studio.name().map(ToString::to_string))
            .collect::<Vec<_>>();
        let has_studios = !studios.is_empty();

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(home_section_title("工作室", cx))
            .when(has_studios, |this| {
                this.child(div().flex().flex_wrap().gap_2().children(
                    studios.into_iter().enumerate().map(|(index, name)| {
                        detail_tag(name.clone(), false, cx).id((
                            gpui::ElementId::from("series-detail-studio-tag"),
                            format!("{name}-{index}"),
                        ))
                    }),
                ))
            })
            .when(!has_studios, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("暂无工作室信息"),
                )
            })
    }

    fn render_series_detail_links_row(
        &self,
        item: &MediaItem,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let links = item
            .external_urls
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|link| Some((link.name()?.to_string(), link.url()?.to_string())))
            .collect::<Vec<_>>();
        let has_links = !links.is_empty();

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(home_section_title("链接", cx))
            .when(has_links, |this| {
                this.child(div().flex().flex_wrap().gap_2().children(
                    links.into_iter().enumerate().map(|(index, (name, url))| {
                        detail_tag(name.clone(), true, cx)
                            .id((
                                gpui::ElementId::from("series-detail-link-tag"),
                                format!("{name}-{index}"),
                            ))
                            .on_click(move |_, _, cx| cx.open_url(&url))
                    }),
                ))
            })
            .when(!has_links, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("暂无外部链接"),
                )
            })
    }
}

fn detail_tag<T>(label: String, clickable: bool, cx: &Context<T>) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(32.0))
        .items_center()
        .rounded_full()
        .border_1()
        .border_color(theme.input_border)
        .bg(theme.dialog_background.opacity(0.86))
        .px_4()
        .text_sm()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.foreground)
        .when(clickable, |this| {
            this.cursor_pointer()
                .hover(move |style| style.bg(theme.secondary_hover))
        })
        .child(label)
}

fn detail_error_with_retry<T>(
    id: &'static str,
    error: gpui::SharedString,
    retry: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &Context<T>,
) -> impl IntoElement {
    let theme = theme::get(cx);
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(div().text_sm().text_color(theme.error).child(error))
        .child(
            div()
                .id(id)
                .rounded_md()
                .px_2()
                .py_1()
                .text_sm()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.input_border_focused)
                .hover(move |style| style.bg(theme.secondary_hover))
                .cursor_pointer()
                .child("重试")
                .on_click(retry),
        )
}

fn detail_select_box<T>(
    label: &'static str,
    value: String,
    enabled: bool,
    cx: &Context<T>,
) -> gpui::Div {
    detail_select_box_with_width(label, value, enabled, DETAIL_SELECT_WIDTH_PX, cx)
}

fn detail_select_box_with_width<T>(
    label: &'static str,
    value: String,
    enabled: bool,
    width: f32,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(34.0))
        .w(px(width))
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

fn season_popup_menu_trigger<T>(
    value: String,
    enabled: bool,
    width: f32,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(32.0))
        .w(px(width))
        .max_w_full()
        .items_center()
        .justify_center()
        .rounded(px(8.0))
        .border_1()
        .border_color(if enabled {
            theme.input_border
        } else {
            theme.input_border.opacity(0.62)
        })
        .bg(theme.dialog_background.opacity(0.88))
        .px_4()
        .text_sm()
        .font_weight(gpui::FontWeight::MEDIUM)
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
        .child(div().min_w_0().truncate().child(value))
}

fn detail_select_menu<T, I, E>(
    id: &'static str,
    option_count: usize,
    width: f32,
    scroll_handle: &ScrollHandle,
    cx: &Context<T>,
    children: I,
) -> impl IntoElement
where
    I: IntoIterator<Item = E>,
    E: IntoElement,
{
    let theme = theme::get(cx);
    let scrollable = detail_select_menu_is_scrollable(option_count);
    let content_scroll_handle = scroll_handle.clone();
    let scrollbar_scroll_handle = scroll_handle.clone();

    div()
        .id(id)
        .absolute()
        .top(px(40.0))
        .left_0()
        .flex()
        .flex_col()
        .w(px(width))
        .max_w_full()
        .when(scrollable, |this| {
            this.h(px(DETAIL_SELECT_MENU_MAX_HEIGHT_PX))
        })
        .overflow_hidden()
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.input_border_focused)
        .bg(theme.dialog_background)
        .shadow_lg()
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(
            div()
                .id((gpui::ElementId::from(id), "content"))
                .flex()
                .flex_col()
                .gap_1()
                .p(px(4.0))
                .when(scrollable, |this| {
                    this.size_full()
                        .overflow_y_scroll()
                        .scrollbar_width(px(SCROLLBAR_WIDTH_PX))
                        .track_scroll(&content_scroll_handle)
                })
                .children(children),
        )
        .when(scrollable, |this| {
            this.child(
                Scrollbar::vertical(&scrollbar_scroll_handle)
                    .id((gpui::ElementId::from(id), "scrollbar"))
                    .edge_inset(px(4.0)),
            )
        })
}

fn detail_select_option<T>(
    label: String,
    selected: bool,
    id: impl Into<gpui::ElementId>,
    cx: &Context<T>,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);
    let show_tooltip = detail_select_label_needs_tooltip(&label);
    let tooltip_label = label.clone();

    div()
        .id(id)
        .flex()
        .flex_none()
        .h(px(DETAIL_SELECT_OPTION_HEIGHT_PX))
        .items_center()
        .rounded(px(6.0))
        .px_1()
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
        .when(show_tooltip, |this| {
            this.tooltip(move |_, cx| text_tooltip(tooltip_label.clone(), cx))
        })
        .child(div().flex_1().min_w_0().truncate().child(label))
}

fn detail_select_label_needs_tooltip(label: &str) -> bool {
    detail_select_label_width_units(label) > DETAIL_SELECT_TOOLTIP_WIDTH_UNITS
}

fn detail_play_button_label(playback_loading: bool, playback_seconds: Option<u64>) -> String {
    if playback_loading {
        "获取播放地址中…".to_string()
    } else if let Some(total_seconds) = playback_seconds {
        let minutes = total_seconds / 60;
        let seconds = total_seconds % 60;
        format!("继续 {minutes}:{seconds:02}")
    } else {
        "播放".to_string()
    }
}

fn detail_select_label_width_units(label: &str) -> usize {
    label
        .chars()
        .map(|character| if character.is_ascii() { 1 } else { 2 })
        .sum::<usize>()
}

fn season_select_width<'a>(labels: impl IntoIterator<Item = &'a str>) -> f32 {
    let label_units = labels
        .into_iter()
        .map(detail_select_label_width_units)
        .max()
        .unwrap_or_else(|| detail_select_label_width_units("请选择"));

    (label_units as f32 * SELECT_TEXT_UNIT_WIDTH_PX + SEASON_SELECT_HORIZONTAL_PADDING_PX)
        .clamp(SEASON_SELECT_MIN_WIDTH_PX, SEASON_SELECT_MAX_WIDTH_PX)
}

fn detail_select_menu_is_scrollable(option_count: usize) -> bool {
    option_count > DETAIL_SELECT_MAX_VISIBLE_OPTIONS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_select_menu_scrolls_only_after_five_options() {
        assert!(!detail_select_menu_is_scrollable(5));
        assert!(detail_select_menu_is_scrollable(6));
    }

    #[test]
    fn detail_select_tooltip_detects_long_ascii_and_cjk_labels() {
        assert!(!detail_select_label_needs_tooltip("English 5.1"));
        assert!(detail_select_label_needs_tooltip(
            "English Dolby Digital Plus 7.1 Atmos Commentary"
        ));
        assert!(detail_select_label_needs_tooltip(
            "简体中文与英文双语特效字幕导演评论版本"
        ));
    }

    #[test]
    fn play_button_label_shows_resume_time_with_two_digit_seconds() {
        assert_eq!(detail_play_button_label(false, Some(905)), "继续 15:05");
        assert_eq!(detail_play_button_label(false, Some(900)), "继续 15:00");
        assert_eq!(detail_play_button_label(false, None), "播放");
        assert_eq!(detail_play_button_label(true, Some(905)), "获取播放地址中…");
    }

    #[test]
    fn season_select_width_clamps_short_and_long_labels() {
        assert_eq!(
            season_select_width(["第一季", "第二季"]),
            SEASON_SELECT_MIN_WIDTH_PX
        );
        assert_eq!(
            season_select_width(["This is an exceptionally long season name for testing"]),
            SEASON_SELECT_MAX_WIDTH_PX
        );
    }
}
