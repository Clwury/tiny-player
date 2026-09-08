use super::*;

impl HomeContent {
    pub(super) fn render_series_detail_episodes_row(
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

                                    let mut episode = episode.clone();
                                    episode.user_data = self
                                        .effective_user_data(
                                            &episode.id,
                                            episode.user_data.as_ref(),
                                        )
                                        .cloned();
                                    episode_card(&episode, image_path, selected, cx)
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
