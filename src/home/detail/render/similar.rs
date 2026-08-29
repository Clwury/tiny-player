use super::*;

impl HomeContent {
    pub(super) fn render_series_detail_similar_section(
        &self,
        detail: &SeriesDetailState,
        viewport_width: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let items = detail.similar_items.as_ref();

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(home_section_title("相似作品", cx))
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
    }

    pub(super) fn render_series_detail_similar_row(
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

    pub(super) fn render_series_detail_links_row(
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
