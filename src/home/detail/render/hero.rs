use super::*;

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
                                .when(detail.item.is_some(), |this| {
                                    this.child(self.render_series_detail_controls(detail, cx))
                                })
                                .when(detail.is_series(), |this| {
                                    this.when_some(detail.seasons.as_ref(), |this, seasons| {
                                        this.child(self.render_series_detail_season_selector(
                                            detail, seasons, cx,
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

    pub(super) fn render_series_detail_hero(
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
        let show_title_fallback = !detail.is_movie()
            && detail
                .item
                .as_ref()
                .is_some_and(|item| item.logo_image_tag().is_none());
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
                    .w(px(760.0))
                    .max_w_full()
                    .flex_col()
                    .gap_3()
                    .text_color(theme.foreground)
                    .when(logo_path.is_some() || show_title_fallback, |this| {
                        this.child(
                            div()
                                .flex()
                                .flex_col()
                                .w_full()
                                .gap_2()
                                .when_some(logo_path.clone(), |this, path| {
                                    this.child(
                                        img(path)
                                            .debug_selector(|| "series-detail-logo".to_string())
                                            .w(px(200.0)),
                                    )
                                })
                                .when(show_title_fallback, |this| {
                                    this.child(
                                        div()
                                            .w_full()
                                            .min_w_0()
                                            .whitespace_normal()
                                            .text_lg()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .child(display_title),
                                    )
                                }),
                        )
                    })
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
            )
    }

    pub(super) fn render_series_detail_metadata_row(
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
}
