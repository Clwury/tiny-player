use super::*;

impl HomeContent {
    pub(super) fn render_series_detail_people_row(
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

    pub(super) fn render_series_detail_studios_row(
        &self,
        item: &MediaItem,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let studios = item
            .studios
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|studio| studio.name().map(ToString::to_string))
            .collect::<Vec<_>>();

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(home_section_title("工作室", cx))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(studios.into_iter().enumerate().map(|(index, name)| {
                        detail_tag(name.clone(), false, cx).id((
                            gpui::ElementId::from("series-detail-studio-tag"),
                            format!("{name}-{index}"),
                        ))
                    })),
            )
    }
}
