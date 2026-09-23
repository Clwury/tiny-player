use super::*;
use crate::ui::paint::device_bounds;
use gpui::{AnyElement, App, Corners, Element, GlobalElementId, InspectorElementId};

/// Lay out the video, subtitles and size observer together using this frame's
/// actual bounds, including during native resize and decoration changes.
pub(super) struct VideoViewport {
    source: Option<RenderSize>,
    corners: Corners<Pixels>,
    content: AnyElement,
}

impl VideoViewport {
    pub(super) fn new(
        source: Option<RenderSize>,
        corners: Corners<Pixels>,
        content: gpui::Div,
    ) -> Self {
        Self {
            source,
            corners,
            content: content
                .debug_selector(|| "playback-video-viewport".to_owned())
                .relative()
                .size_full()
                .overflow_hidden()
                .into_any_element(),
        }
    }
}

impl IntoElement for VideoViewport {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for VideoViewport {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, ()) {
        let style = gpui::Style {
            size: gpui::size(gpui::relative(1.0).into(), gpui::relative(1.0).into()),
            ..Default::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let viewport = safe_video_viewport(bounds, self.source, self.corners, window);
        self.content
            .layout_as_root(viewport.size.into(), window, cx);
        self.content.prepaint_at(viewport.origin, window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.content.paint(window, cx);
    }
}

fn safe_video_viewport(
    bounds: Bounds<Pixels>,
    source: Option<RenderSize>,
    corners: Corners<Pixels>,
    window: &Window,
) -> Bounds<Pixels> {
    let display_scale = window.scale_factor();
    let viewport = device_bounds(window, bounds);
    let logical_bounds = |bounds: Bounds<i32>| bounds.map(|value| px(value as f32 / display_scale));
    let bounds = logical_bounds(viewport);
    let Some(source) = source else {
        return bounds;
    };
    let Some(fitted) = aspect_fit_bounds(bounds, source) else {
        return bounds;
    };
    // Keep the entire image outside the bottom corner squares. Natural
    // letterboxing or pillarboxing already supplies this clearance in most
    // cases. The titlebar owns the top window corners.
    let corners = corners.clamp_radii_for_quad_size(bounds.size);
    let radius =
        (f32::from(corners.bottom_left.max(corners.bottom_right)) * display_scale).ceil() as i32;
    let image = device_bounds(window, fitted);
    if viewport.bottom() - image.bottom() >= radius
        || (image.left() - viewport.left()).min(viewport.right() - image.right()) >= radius
    {
        return bounds;
    }

    // A centered rectangle avoids both corner squares by clearing either the
    // bottom edge or the side edges. Compare the two largest safe rectangles
    // and choose the one that fits more video at its original aspect ratio.
    // Whole-device-pixel insets survive GPUI's origin and size snapping.
    let horizontal_inset = radius.min(viewport.size.width / 2);
    let vertical_inset = radius.min(viewport.size.height / 2);
    let candidates = [
        Bounds::from_corners(
            gpui::point(viewport.left() + horizontal_inset, viewport.top()),
            gpui::point(viewport.right() - horizontal_inset, viewport.bottom()),
        ),
        Bounds::from_corners(
            gpui::point(viewport.left(), viewport.top() + vertical_inset),
            gpui::point(viewport.right(), viewport.bottom() - vertical_inset),
        ),
    ];
    candidates
        .into_iter()
        .map(logical_bounds)
        .max_by_key(|candidate| {
            aspect_fit_bounds(*candidate, source)
                .map(|fitted| fitted.size.width)
                .unwrap_or_default()
        })
        .unwrap_or(bounds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{ScaledPixels, TestAppContext, point, size};

    fn outside_bottom_corner_squares(
        viewport: Bounds<i32>,
        image: Bounds<i32>,
        radius: f32,
    ) -> bool {
        if image.left() < viewport.left()
            || image.right() > viewport.right()
            || image.top() < viewport.top()
            || image.bottom() > viewport.bottom()
        {
            return false;
        }
        // No image vertex may enter either radius-sized corner square.
        for x in [image.left(), image.right()] {
            for y in [image.top(), image.bottom()] {
                let bottom_distance = (viewport.bottom() - y) as f32;
                let side_distance = (x - viewport.left()).min(viewport.right() - x) as f32;
                if bottom_distance < radius && side_distance < radius {
                    return false;
                }
            }
        }
        true
    }

    #[gpui::test]
    fn natural_black_bars_use_full_width_or_height_without_extra_padding(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| TestViewport::default());
        cx.update(|window, _| {
            for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
                window.set_scale_factor(scale);
                let bounds = window.pixel_snap_bounds(Bounds::new(
                    point(px(1.0), px(36.0)),
                    size(px(1000.0), px(700.0)),
                ));
                for source in [
                    RenderSize {
                        width: 1920,
                        height: 1080,
                    },
                    RenderSize {
                        width: 1080,
                        height: 1920,
                    },
                ] {
                    assert_eq!(
                        safe_video_viewport(bounds, Some(source), Corners::all(px(9.0)), window),
                        bounds
                    );
                }
                let matching = RenderSize {
                    width: 1000,
                    height: 700,
                };
                assert_eq!(
                    safe_video_viewport(bounds, Some(matching), Corners::default(), window),
                    bounds
                );
                assert_eq!(
                    safe_video_viewport(bounds, None, Corners::all(px(9.0)), window),
                    bounds
                );
                assert_eq!(
                    safe_video_viewport(
                        bounds,
                        Some(RenderSize {
                            width: 0,
                            height: 1
                        }),
                        Corners::all(px(9.0)),
                        window
                    ),
                    bounds
                );
            }
        });
    }

    #[gpui::test]
    fn near_matching_aspects_choose_the_larger_straight_edge_fit(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| TestViewport::default());
        cx.update(|window, _| {
            for display_scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
                window.set_scale_factor(display_scale);
                for (width, height) in [
                    (1000.0, 700.0),
                    (700.0, 1000.0),
                    (1001.3, 703.7),
                    (53.0, 31.0),
                ] {
                    let bounds =
                        Bounds::new(point(px(1.25), px(36.4)), size(px(width), px(height)));
                    let viewport = device_bounds(window, bounds);
                    let snapped = viewport.map(|value| px(value as f32 / display_scale));
                    for adjustment in [0.99, 1.0, 1.01] {
                        let source = RenderSize {
                            width: (width * 100.0 * adjustment) as u32,
                            height: (height * 100.0) as u32,
                        };
                        let safe = safe_video_viewport(
                            bounds,
                            Some(source),
                            Corners::all(px(9.0)),
                            window,
                        );
                        let fitted = aspect_fit_bounds(safe, source).unwrap();
                        let inset = px((9.0_f32 * display_scale).ceil() / display_scale);
                        // Independently compare the maximum source scale in
                        // each straight-edged safe area. No circle equation.
                        let side_fit = ((snapped.size.width - 2.0 * inset)
                            / px(source.width as f32))
                        .min(snapped.size.height / px(source.height as f32));
                        let bottom_fit = (snapped.size.width / px(source.width as f32))
                            .min((snapped.size.height - 2.0 * inset) / px(source.height as f32));
                        let expected_scale = side_fit.max(bottom_fit);
                        assert!(
                            (f32::from(fitted.size.width) - source.width as f32 * expected_scale)
                                .abs()
                                < 0.001
                        );
                        assert!(
                            (f32::from(fitted.size.height) - source.height as f32 * expected_scale)
                                .abs()
                                < 0.001
                        );
                        assert!((f32::from(fitted.center().x - snapped.center().x)).abs() < 0.001);
                        assert!((f32::from(fitted.center().y - snapped.center().y)).abs() < 0.001);
                        assert!(outside_bottom_corner_squares(
                            viewport,
                            device_bounds(window, fitted),
                            9.0 * display_scale
                        ));
                        // One axis remains flush with the available frame;
                        // the other alone supplies the corner clearance.
                        assert!(
                            safe.size.width == snapped.size.width
                                || safe.size.height == snapped.size.height
                        );
                    }
                }
            }
        });
    }

    #[gpui::test]
    fn equal_aspect_video_does_not_expand_into_the_corner_arc(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| TestViewport::default());
        cx.update(|window, _| {
            window.set_scale_factor(1.0);
            for (width, height, expected_width, expected_height) in
                [(1000, 700, 982.0, 687.4), (700, 1000, 687.4, 982.0)]
            {
                let bounds = Bounds::new(
                    point(px(0.0), px(0.0)),
                    size(px(width as f32), px(height as f32)),
                );
                let source = RenderSize { width, height };
                let safe = safe_video_viewport(bounds, Some(source), Corners::all(px(9.0)), window);
                let fitted = aspect_fit_bounds(safe, source).unwrap();
                assert!((f32::from(fitted.size.width) - expected_width).abs() < 0.001);
                assert!((f32::from(fitted.size.height) - expected_height).abs() < 0.001);
                for corners in [
                    Corners {
                        bottom_left: px(9.0),
                        ..Default::default()
                    },
                    Corners {
                        bottom_right: px(9.0),
                        ..Default::default()
                    },
                ] {
                    assert_eq!(
                        safe_video_viewport(bounds, Some(source), corners, window),
                        safe
                    );
                }
            }
        });
    }

    struct TestViewport {
        source: RenderSize,
        radius: Pixels,
    }

    impl Default for TestViewport {
        fn default() -> Self {
            Self {
                source: RenderSize {
                    width: 1000,
                    height: 700,
                },
                radius: px(9.0),
            }
        }
    }

    impl Render for TestViewport {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let source = self.source;
            div().relative().size_full().child(VideoViewport::new(
                Some(source),
                Corners::all(self.radius),
                div().child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, _| {
                            window.paint_quad(gpui::fill(
                                aspect_fit_bounds(bounds, source).unwrap(),
                                gpui::white(),
                            ));
                        },
                    )
                    .size_full(),
                ),
            ))
        }
    }

    #[gpui::test]
    fn layout_adapts_to_resize_source_changes_and_square_window_transitions(
        cx: &mut TestAppContext,
    ) {
        let (root, cx) = cx.add_window_view(|_, _| TestViewport::default());
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
            for (width, height) in [(1000.0, 700.0), (1001.3, 703.7)] {
                for (source, radius) in [
                    (
                        RenderSize {
                            width: 1000,
                            height: 700,
                        },
                        px(9.0),
                    ),
                    (
                        RenderSize {
                            width: 1920,
                            height: 1080,
                        },
                        px(9.0),
                    ),
                    (
                        RenderSize {
                            width: 1080,
                            height: 1920,
                        },
                        px(9.0),
                    ),
                    (
                        RenderSize {
                            width: 1000,
                            height: 700,
                        },
                        px(0.0),
                    ),
                    (
                        RenderSize {
                            width: 1000,
                            height: 700,
                        },
                        px(9.0),
                    ),
                ] {
                    root.update(cx, |root, cx| {
                        root.source = source;
                        root.radius = radius;
                        cx.notify();
                    });
                    cx.simulate_resize(size(px(width), px(height)));
                    cx.update(|window, _| window.set_scale_factor(scale));
                    cx.run_until_parked();
                    let content = cx.debug_bounds("playback-video-viewport").unwrap();
                    cx.update(|window, _| {
                        let bounds = Bounds::new(point(px(0.0), px(0.0)), window.viewport_size());
                        let expected =
                            safe_video_viewport(bounds, Some(source), Corners::all(radius), window);
                        for delta in [
                            content.left() - expected.left(),
                            content.top() - expected.top(),
                            content.right() - expected.right(),
                            content.bottom() - expected.bottom(),
                        ] {
                            assert!(
                                f32::from(delta).abs() < 0.001,
                                "{content:?} != {expected:?}"
                            );
                        }
                        let quads = window.painted_quads();
                        assert_eq!(quads.len(), 1);
                        let painted = quads[0]
                            .bounds
                            .map(|value: ScaledPixels| value.0.round() as i32);
                        assert!(outside_bottom_corner_squares(
                            device_bounds(window, bounds),
                            painted,
                            f32::from(radius) * scale
                        ));
                        assert_eq!(quads[0].corner_radii, Corners::default());
                    });
                }
            }
        }
    }
}
