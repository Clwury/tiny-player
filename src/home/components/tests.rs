use super::*;
use super::{episode::episode_card_image, poster::resume_item_card_image};

#[gpui::test]
fn carousel_buttons_only_block_cards_under_visible_controls(cx: &mut gpui::TestAppContext) {
    use gpui::{Modifiers, Render, point};

    use crate::home::carousel::CarouselState;

    #[derive(Default)]
    struct CarouselHover {
        carousel: CarouselState,
        controls_enabled: bool,
        card_clicks: [usize; 2],
        button_clicks: [usize; 2],
    }

    impl Render for CarouselHover {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let visible = self.controls_enabled && self.carousel.controls_visible(true);
            let theme = theme::get(cx);
            div().size_full().p_4().child(
                div()
                    .id("carousel-hover-row")
                    .debug_selector(|| "carousel-hover-row".into())
                    .relative()
                    .w(px(320.0))
                    .overflow_hidden()
                    .on_hover(cx.listener(|view, hovered, _, cx| {
                        if view.carousel.set_hovered(*hovered) {
                            cx.notify();
                        }
                    }))
                    .child(div().flex().gap_4().children((0..2).map(|index| {
                        let item = serde_json::from_value(serde_json::json!({
                            "Id": index.to_string(), "Name": "Movie", "Type": "Movie"
                        }))
                        .unwrap();
                        user_item_card(UserItemCardVm::new(&item, None, false), None, cx)
                            .id(("carousel-hover-card", index))
                            .cursor_pointer()
                            .on_click(cx.listener(move |view, _, _, _| {
                                view.card_clicks[index] += 1;
                            }))
                    })))
                    .children(
                        [
                            ("carousel-hover-left", "icons/chevron-left.svg", false),
                            ("carousel-hover-right", "icons/chevron-right.svg", true),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (id, icon, align_right))| {
                            carousel_button(
                                id,
                                icon,
                                align_right,
                                visible,
                                theme,
                                cx.listener(|view, hovered, _, cx| {
                                    if view.carousel.set_controls_hovered(*hovered) {
                                        cx.notify();
                                    }
                                }),
                                cx.listener(move |view, _, _, _| {
                                    view.button_clicks[index] += 1;
                                }),
                            )
                        }),
                    ),
            )
        }
    }

    cx.update(|cx| theme::set(theme::ColorTheme::Latte, cx));
    let (view, cx) = cx.add_window_view(|_, _| CarouselHover::default());
    cx.run_until_parked();
    let row = cx.debug_bounds("carousel-hover-row").unwrap();
    let card_hovered_at = |cx: &mut gpui::VisualTestContext,
                           position: gpui::Point<gpui::Pixels>| {
        cx.update(|window, cx| {
            let scale = window.scale_factor();
            window.painted_quads().iter().any(|quad| {
                quad.background == theme::get(cx).secondary_hover.into()
                    && quad.bounds.size.width
                        == px(HOME_ITEM_CARD_WIDTH_PX + HOME_ITEM_CARD_PADDING_PX * 2.0)
                            .scale(scale)
                    && quad.bounds.contains(&position.scale(scale))
            })
        })
    };

    // The full-height edge strips must leave the cover, title, and the space
    // beside each button interactive, even when the controls are visible.
    for visible in [false, true] {
        view.update(cx, |view, cx| {
            view.controls_enabled = visible;
            cx.notify();
        });
        for (index, edge, direction) in [(0, row.left(), 1.0), (1, row.right(), -1.0)] {
            for (inset, y) in [
                (20.0, row.top() + px(20.0)),
                (20.0, row.bottom() - px(12.0)),
                (4.0, row.center().y),
                (44.0, row.center().y),
            ] {
                let position = point(edge + px(inset * direction), y);
                cx.simulate_mouse_move(position, None, Modifiers::default());
                cx.run_until_parked();
                assert!(
                    card_hovered_at(cx, position),
                    "visible={visible}, {position:?}"
                );
                let before = view.read_with(cx, |view, _| view.card_clicks[index]);
                cx.simulate_click(position, Modifiers::default());
                cx.run_until_parked();
                view.read_with(cx, |view, _| {
                    assert_eq!(view.card_clicks[index], before + 1);
                    assert_eq!(view.button_clicks, [0, 0]);
                });
            }
        }
    }

    for (index, x) in [(0, row.left() + px(24.0)), (1, row.right() - px(24.0))] {
        let position = point(x, row.center().y);
        // Hidden controls must also leave their former button area interactive.
        view.update(cx, |view, cx| {
            view.controls_enabled = false;
            cx.notify();
        });
        cx.simulate_mouse_move(position, None, Modifiers::default());
        cx.run_until_parked();
        assert!(card_hovered_at(cx, position));
        let before = view.read_with(cx, |view, _| view.card_clicks[index]);
        cx.simulate_click(position, Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.card_clicks[index]),
            before + 1
        );

        // Showing a button under the stationary pointer must keep it visible
        // and reserve its clicks for scrolling, without activating the card.
        view.update(cx, |view, cx| {
            view.controls_enabled = true;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(!card_hovered_at(cx, position));
        assert!(view.read_with(cx, |view, _| view.carousel.controls_visible(true)));
        let cards_before = view.read_with(cx, |view, _| view.card_clicks);
        cx.simulate_click(position, Modifiers::default());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.button_clicks[index], 1);
            assert_eq!(view.card_clicks, cards_before);
        });
        cx.simulate_mouse_move(point(px(2.0), px(2.0)), None, Modifiers::default());
        cx.run_until_parked();
        assert!(!view.read_with(cx, |view, _| view.carousel.controls_visible(true)));
    }
}

#[gpui::test]
fn latte_cards_change_fill_over_the_image_and_label(cx: &mut gpui::TestAppContext) {
    use gpui::{Modifiers, Render, point};

    struct CardHover(bool);

    impl Render for CardHover {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let item = serde_json::json!({"Id": "1", "Name": "Movie", "Type": "Movie"});
            let card = if self.0 {
                episode_card(
                    EpisodeCardVm::new(&serde_json::from_value(item).unwrap(), None, true),
                    None,
                    cx,
                )
            } else {
                user_item_card(
                    UserItemCardVm::new(&serde_json::from_value(item).unwrap(), None, true),
                    None,
                    cx,
                )
            };
            div()
                .size_full()
                .bg(theme::get(cx).background)
                .p_4()
                .child(card.id("card").debug_selector(|| "card".into()))
        }
    }

    cx.update(|cx| theme::set(theme::ColorTheme::Latte, cx));
    let (view, cx) = cx.add_window_view(|_, _| CardHover(false));
    for selected in [false, true] {
        view.update(cx, |view, cx| {
            view.0 = selected;
            cx.notify();
        });
        cx.run_until_parked();
        cx.simulate_mouse_move(point(px(2.0), px(2.0)), None, Modifiers::default());
        cx.run_until_parked();
        let bounds = cx.debug_bounds("card").unwrap();
        for position in [
            bounds.origin + point(px(20.0), px(20.0)),
            point(bounds.center().x, bounds.bottom() - px(12.0)),
        ] {
            cx.simulate_mouse_move(position, None, Modifiers::default());
            cx.run_until_parked();
            assert!(
                cx.update(|window, cx| {
                    let theme = theme::get(cx);
                    let expected = if selected {
                        theme.element_selected_hover
                    } else {
                        theme.secondary_hover
                    };
                    window
                        .painted_quads()
                        .iter()
                        .any(|quad| quad.background == expected.into())
                }),
                "selected={selected} position={position:?} bounds={bounds:?}"
            );
        }
    }
}

#[gpui::test]
fn resume_progress_matches_cover_corners_and_stays_inside_visible_bounds(
    cx: &mut gpui::TestAppContext,
) {
    assert_cover_progress_bounds(cx, false);
}

#[gpui::test]
fn detail_episode_progress_matches_cover_corners_and_stays_inside_visible_bounds(
    cx: &mut gpui::TestAppContext,
) {
    assert_cover_progress_bounds(cx, true);
}

fn assert_cover_progress_bounds(cx: &mut gpui::TestAppContext, detail_episode: bool) {
    use gpui::{Render, ScaledPixels};

    struct ProgressCard {
        detail_episode: bool,
        fraction: Option<f32>,
        rem_size: f32,
        visible_width: f32,
    }

    impl Render for ProgressCard {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            window.set_rem_size(px(self.rem_size));
            div().size_full().p(px(16.0)).child(
                div()
                    .w(px(self.visible_width))
                    .overflow_hidden()
                    .child(if self.detail_episode {
                        episode_card_image(None, self.fraction, cx).into_any_element()
                    } else {
                        resume_item_card_image(None, self.fraction, false, cx).into_any_element()
                    }),
            )
        }
    }

    cx.update(theme::init);
    let cover_width = if detail_episode {
        DETAIL_EPISODE_CARD_WIDTH_PX
    } else {
        USER_VIEW_CARD_WIDTH_PX
    };
    let (card, cx) = cx.add_window_view(|_, _| ProgressCard {
        detail_episode,
        fraction: None,
        rem_size: 16.0,
        visible_width: cover_width,
    });

    for rem_size in [16.0, 20.0] {
        for visible_width in [cover_width, 180.0] {
            for fraction in [None, Some(-0.1), Some(0.0), Some(0.5), Some(1.0), Some(1.1)] {
                card.update(cx, |card, cx| {
                    card.fraction = fraction;
                    card.rem_size = rem_size;
                    card.visible_width = visible_width;
                    cx.notify();
                });
                cx.run_until_parked();
                cx.update(|window, cx| {
                    let theme = theme::get(cx);
                    let quads = window.painted_quads();
                    let cover = quads
                        .iter()
                        .find(|quad| quad.background == theme.input_background.into())
                        .expect("cover background is painted");
                    let track = quads
                        .iter()
                        .find(|quad| quad.background == theme.background.opacity(0.72).into());
                    let progress = quads
                        .iter()
                        .find(|quad| quad.background == theme.input_border_focused.into());

                    assert_eq!(track.is_some(), fraction.is_some());
                    assert_eq!(
                        progress.is_some(),
                        fraction.is_some_and(|value| value > 0.0)
                    );
                    for (quad, width) in
                        track
                            .map(|quad| (quad, visible_width))
                            .into_iter()
                            .chain(progress.map(|quad| {
                                let width = cover_width * fraction.unwrap().clamp(0.0, 1.0);
                                (quad, width.min(visible_width))
                            }))
                    {
                        assert_eq!(quad.bounds, cover.bounds);
                        assert_eq!(quad.corner_radii, cover.corner_radii);
                        let mask = quad.content_mask.bounds;
                        assert_eq!(mask.left(), cover.bounds.left());
                        assert_eq!(mask.bottom(), cover.bounds.bottom());
                        assert_eq!(mask.size.width, ScaledPixels(width * window.scale_factor()));
                        assert_eq!(mask.size.height, ScaledPixels(4.0 * window.scale_factor()));
                    }
                });
            }
        }
    }
}
