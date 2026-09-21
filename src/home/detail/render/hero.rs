use std::{path::Path, sync::Arc};

use super::*;
use gpui::{
    App, ImgResourceLoader, RenderOnce, Resource, black, linear_color_stop, linear_gradient,
};

const HERO_TEXT_SCRIM_OPACITY: f32 = 0.72;
const HERO_BOTTOM_SCRIM_HEIGHT_PX: f32 = 120.0;
const HERO_LOGO_HEIGHT_PX: f32 = 80.0;
const HERO_LOGO_MAX_WIDTH_PX: f32 = 480.0;
const HERO_LOGO_COMPACT_WIDTH_PX: f32 = 200.0;
const HERO_LOGO_MAX_HEIGHT_PX: f32 = 200.0;

#[derive(IntoElement)]
struct HeroLogo {
    path: Arc<Path>,
    max_height: f32,
}

impl RenderOnce for HeroLogo {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // Reuse the original image's decoder/cache to inspect its dimensions.
        // No separate image processing or synchronous file reads are needed.
        let image = window.use_asset::<ImgResourceLoader>(&Resource::Path(self.path.clone()), cx);
        let max_height = self.max_height.max(HERO_LOGO_HEIGHT_PX);
        let height = image
            .and_then(Result::ok)
            .map(|image| {
                let size = image.size(0);
                let ratio = size.width.0.max(1) as f32 / size.height.0.max(1) as f32;
                (HERO_LOGO_COMPACT_WIDTH_PX / ratio).clamp(HERO_LOGO_HEIGHT_PX, max_height)
            })
            .unwrap_or(HERO_LOGO_HEIGHT_PX);

        img(self.path)
            .debug_selector(|| "series-detail-logo".to_string())
            .h(px(height))
            .max_w_full()
            .object_fit(gpui::ObjectFit::Contain)
    }
}

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
            .on_mouse_down_out(cx.listener(Self::close_series_detail_select))
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
                                .when(detail.item.is_some(), |this| {
                                    this.child(self.render_series_detail_controls(detail, cx))
                                })
                                .when(detail.is_movie(), |this| {
                                    this.when_some(
                                        detail
                                            .item
                                            .as_ref()
                                            .and_then(hero_metadata::movie_overview),
                                        |this, overview| {
                                            this.child(
                                                div()
                                                    .debug_selector(|| {
                                                        "movie-detail-overview".into()
                                                    })
                                                    .w_full()
                                                    .text_sm()
                                                    .line_height(px(22.0))
                                                    .text_color(theme.muted_foreground)
                                                    .text_ellipsis()
                                                    .line_clamp(3)
                                                    .child(overview),
                                            )
                                        },
                                    )
                                })
                                .when(detail.is_series(), |this| {
                                    this.when_some(detail.seasons.as_ref(), |this, seasons| {
                                        this.child(self.render_series_detail_season_selector(
                                            detail, seasons, window, cx,
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
                                    .when_some(
                                        detail
                                            .episodes
                                            .as_ref()
                                            .filter(|episodes| !episodes.items.is_empty()),
                                        |this, episodes| {
                                            this.child(self.render_series_detail_episodes_row(
                                                detail,
                                                episodes,
                                                main_content_width,
                                                cx,
                                            ))
                                        },
                                    )
                                    .when(
                                        detail.effects.episodes == LoadState::Loaded
                                            && detail
                                                .episodes
                                                .as_ref()
                                                .is_none_or(|episodes| episodes.items.is_empty()),
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
                                .when(
                                    detail
                                        .similar_items
                                        .as_ref()
                                        .is_some_and(|items| !items.items.is_empty()),
                                    |this| {
                                        this.child(self.render_series_detail_similar_section(
                                            detail,
                                            main_content_width,
                                            cx,
                                        ))
                                    },
                                )
                                .when_some(detail.item.as_ref(), |this, item| {
                                    this.when(has_studios(item), |this| {
                                        this.child(self.render_series_detail_studios_row(item, cx))
                                    })
                                    .children(self.render_series_detail_links_row(item, cx))
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
            .debug_selector(|| "series-detail-back-button".to_string())
            .absolute()
            .left_4()
            // Match sidebar p_3 and its 32px button centered in a 36px title row.
            .top_3()
            .mt(px(2.0))
            .flex()
            .size(px(32.0))
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor_pointer()
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

    pub(super) fn render_series_detail_hero(
        &self,
        detail: &SeriesDetailState,
        hero_height: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::media_overlay(cx);
        let backdrop_path = detail
            .item
            .as_ref()
            .and_then(|item| self.image_path_for_series_backdrop(item));
        let logo_path = detail
            .item
            .as_ref()
            .and_then(|item| self.image_path_for_series_logo(item));
        let episode_line = detail.hero_line();
        let metadata = hero_metadata::hero_metadata_label(detail);
        // Leave room for the information row, rating badges, gaps and bottom
        // padding, plus the episode line on series pages, even in short windows.
        let logo_max_height = (hero_height * 0.5)
            .min(HERO_LOGO_MAX_HEIGHT_PX)
            .min(hero_height - if detail.is_series() { 140.0 } else { 104.0 });

        div()
            .debug_selector(|| "series-detail-hero".to_string())
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
                    // Shade the text side while keeping the artwork on the right clear.
                    .debug_selector(|| "series-detail-hero-side-scrim".to_string())
                    .bg(linear_gradient(
                        90.0,
                        linear_color_stop(black().opacity(HERO_TEXT_SCRIM_OPACITY), 0.0),
                        linear_color_stop(black().opacity(0.0), 1.0),
                    )),
            )
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .h(px(hero_height.min(HERO_BOTTOM_SCRIM_HEIGHT_PX)))
                    .debug_selector(|| "series-detail-hero-bottom-scrim".to_string())
                    // Fade across the entire height to avoid a flat dark band
                    // where the gradient used to reach its final opacity early.
                    .bg(linear_gradient(
                        180.0,
                        linear_color_stop(black().opacity(0.0), 0.0),
                        linear_color_stop(black().opacity(HERO_TEXT_SCRIM_OPACITY), 1.0),
                    )),
            )
            .child(
                div()
                    .absolute()
                    .left_6()
                    .right_6()
                    .bottom_6()
                    .flex()
                    .max_w(px(760.0))
                    .flex_col()
                    .gap_3()
                    .text_color(theme.foreground)
                    .when_some(logo_path, |this, path| {
                        this.child(
                            div()
                                .flex()
                                .w_full()
                                .max_w(px(HERO_LOGO_MAX_WIDTH_PX))
                                .child(HeroLogo {
                                    path,
                                    max_height: logo_max_height,
                                }),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            // Keep this row's height stable while source/episode data loads.
                            .child(
                                div()
                                    .debug_selector(|| "series-detail-video-metadata".into())
                                    .flex_none()
                                    .h(px(24.0))
                                    .w_full()
                                    .text_sm()
                                    .line_height(px(24.0))
                                    .truncate()
                                    .when_some(metadata, |this, metadata| this.child(metadata)),
                            )
                            // Reserve the episode line before asynchronous episode data arrives.
                            // This bottom-aligned stack must keep the logo at the same height.
                            .when(detail.is_series(), |this| {
                                this.child(
                                    div()
                                        .debug_selector(|| "series-detail-episode-line".to_string())
                                        .flex()
                                        .flex_none()
                                        .h(px(24.0))
                                        .w_full()
                                        .max_w(px(760.0))
                                        .items_center()
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .text_base()
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .when_some(episode_line, |this, line| this.child(line)),
                                )
                            })
                            .when_some(detail.item.as_ref(), |this, item| {
                                this.child(self.render_series_detail_metadata_row(item, cx))
                            }),
                    ),
            )
    }

    pub(super) fn render_series_detail_metadata_row(
        &self,
        item: &MediaItem,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::media_overlay(cx);
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
}
