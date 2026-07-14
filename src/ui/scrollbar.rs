use std::{cell::Cell, ops::Deref, panic::Location, rc::Rc};

use gpui::{
    App, Bounds, ContentMask, CursorStyle, Element, ElementId, GlobalElementId, Hitbox,
    HitboxBehavior, InspectorElementId, IntoElement, LayoutId, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Position, ScrollHandle, Style, Window, fill, point, px, relative, size,
};

use crate::theme;

pub(crate) const SCROLLBAR_WIDTH_PX: f32 = 6.0;

const SCROLLBAR_WIDTH: Pixels = px(SCROLLBAR_WIDTH_PX);
const MIN_THUMB_HEIGHT_PX: f32 = 32.0;
const DEFAULT_EDGE_INSET: Pixels = px(4.0);

#[derive(Clone, Copy, Debug, Default)]
struct ScrollbarStateInner {
    hovered_thumb: bool,
    dragging: bool,
    drag_offset_y: Pixels,
}

#[derive(Clone, Debug, Default)]
struct ScrollbarState(Rc<Cell<ScrollbarStateInner>>);

impl Deref for ScrollbarState {
    type Target = Rc<Cell<ScrollbarStateInner>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Clone, Copy, Debug)]
struct VerticalScrollbarMetrics {
    thumb_top: f32,
    thumb_height: f32,
    max_offset: f32,
}

fn vertical_scrollbar_metrics(
    track_height: f32,
    viewport_height: f32,
    max_offset: f32,
    scroll_top: f32,
) -> Option<VerticalScrollbarMetrics> {
    if track_height <= 0.0 || viewport_height <= 0.0 || max_offset <= 0.0 {
        return None;
    }

    let content_height = viewport_height + max_offset;
    let thumb_height = (viewport_height / content_height * track_height)
        .max(MIN_THUMB_HEIGHT_PX)
        .min(track_height);
    let thumb_range = (track_height - thumb_height).max(0.0);
    let thumb_top = scroll_top.clamp(0.0, max_offset) / max_offset * thumb_range;

    Some(VerticalScrollbarMetrics {
        thumb_top,
        thumb_height,
        max_offset,
    })
}

#[derive(Clone)]
pub(crate) struct Scrollbar {
    id: ElementId,
    scroll_handle: ScrollHandle,
    edge_inset: Pixels,
    right_inset: Pixels,
}

impl Scrollbar {
    #[track_caller]
    pub(crate) fn vertical(scroll_handle: &ScrollHandle) -> Self {
        Self {
            id: ElementId::CodeLocation(*Location::caller()),
            scroll_handle: scroll_handle.clone(),
            edge_inset: DEFAULT_EDGE_INSET,
            right_inset: px(0.0),
        }
    }

    pub(crate) fn id(mut self, id: impl Into<ElementId>) -> Self {
        self.id = id.into();
        self
    }

    pub(crate) fn edge_inset(mut self, inset: Pixels) -> Self {
        self.edge_inset = inset.max(px(0.0));
        self
    }

    pub(crate) fn right_inset(mut self, inset: Pixels) -> Self {
        self.right_inset = inset.max(px(0.0));
        self
    }
}

impl IntoElement for Scrollbar {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

struct ScrollbarGeometry {
    track_bounds: Bounds<Pixels>,
    thumb_bounds: Bounds<Pixels>,
    metrics: VerticalScrollbarMetrics,
}

fn vertical_scrollbar_track_bounds(
    bounds: Bounds<Pixels>,
    edge_inset: Pixels,
    right_inset: Pixels,
) -> Bounds<Pixels> {
    let track_height = (bounds.size.height - edge_inset * 2.0).max(px(0.0));
    let max_right_inset = (bounds.size.width - SCROLLBAR_WIDTH).max(px(0.0));
    let right_inset = right_inset.min(max_right_inset);

    Bounds::new(
        point(
            bounds.right() - right_inset - SCROLLBAR_WIDTH,
            bounds.top() + edge_inset,
        ),
        size(SCROLLBAR_WIDTH, track_height),
    )
}

pub(crate) struct ScrollbarPrepaintState {
    state: ScrollbarState,
    hitbox: Option<Hitbox>,
    geometry: Option<ScrollbarGeometry>,
}

impl Element for Scrollbar {
    type RequestLayoutState = ();
    type PrepaintState = ScrollbarPrepaintState;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style {
            position: Position::Absolute,
            ..Style::default()
        };
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let state = window
            .use_state(cx, |_, _| ScrollbarState::default())
            .read(cx)
            .clone();
        let track_bounds =
            vertical_scrollbar_track_bounds(bounds, self.edge_inset, self.right_inset);
        let track_height = track_bounds.size.height;
        let viewport_height = self.scroll_handle.bounds().size.height;
        let max_offset = self.scroll_handle.max_offset().height;
        let scroll_top = (-self.scroll_handle.offset().y).clamp(px(0.0), max_offset);
        let metrics = vertical_scrollbar_metrics(
            f32::from(track_height),
            f32::from(viewport_height),
            f32::from(max_offset),
            f32::from(scroll_top),
        );

        let Some(metrics) = metrics else {
            return ScrollbarPrepaintState {
                state,
                hitbox: None,
                geometry: None,
            };
        };

        let thumb_bounds = Bounds::new(
            point(
                track_bounds.left(),
                track_bounds.top() + px(metrics.thumb_top),
            ),
            size(SCROLLBAR_WIDTH, px(metrics.thumb_height)),
        );
        let hitbox = window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.insert_hitbox(track_bounds, HitboxBehavior::Normal)
        });

        ScrollbarPrepaintState {
            state,
            hitbox: Some(hitbox),
            geometry: Some(ScrollbarGeometry {
                track_bounds,
                thumb_bounds,
                metrics,
            }),
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (Some(hitbox), Some(geometry)) = (&prepaint.hitbox, &prepaint.geometry) else {
            return;
        };
        let state = prepaint.state.clone();
        let state_value = state.get();
        let theme = theme::get(cx);
        let track_color = theme.input_border.opacity(0.18);
        let thumb_color = theme.muted_foreground.opacity(0.68);

        window.set_cursor_style(
            if state_value.dragging {
                CursorStyle::ClosedHand
            } else if state_value.hovered_thumb {
                CursorStyle::OpenHand
            } else {
                CursorStyle::Arrow
            },
            hitbox,
        );
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.paint_quad(fill(geometry.track_bounds, track_color).corner_radii(px(6.0)));
            window.paint_quad(fill(geometry.thumb_bounds, thumb_color).corner_radii(px(6.0)));
        });

        let view_id = window.current_view();
        let track_bounds = geometry.track_bounds;
        let thumb_bounds = geometry.thumb_bounds;
        let metrics = geometry.metrics;
        let scroll_handle = self.scroll_handle.clone();
        window.on_mouse_event({
            let state = state.clone();
            move |event: &MouseDownEvent, phase, _, cx| {
                if !phase.bubble() || !track_bounds.contains(&event.position) {
                    return;
                }
                cx.stop_propagation();

                if thumb_bounds.contains(&event.position) {
                    let mut next = state.get();
                    next.dragging = true;
                    next.drag_offset_y = event.position.y - thumb_bounds.top();
                    state.set(next);
                } else {
                    let thumb_range =
                        (f32::from(track_bounds.size.height) - metrics.thumb_height).max(0.0);
                    if thumb_range > 0.0 {
                        let percentage = ((f32::from(event.position.y - track_bounds.top())
                            - metrics.thumb_height / 2.0)
                            / thumb_range)
                            .clamp(0.0, 1.0);
                        let offset = scroll_handle.offset();
                        scroll_handle
                            .set_offset(point(offset.x, px(-(metrics.max_offset * percentage))));
                    }
                }
                cx.notify(view_id);
            }
        });

        let scroll_handle = self.scroll_handle.clone();
        window.on_mouse_event({
            let state = state.clone();
            move |event: &MouseMoveEvent, _, _, cx| {
                let mut next = state.get();
                let hovered_thumb = thumb_bounds.contains(&event.position);
                let mut changed = next.hovered_thumb != hovered_thumb;
                next.hovered_thumb = hovered_thumb;

                if next.dragging && event.dragging() {
                    cx.stop_propagation();
                    let thumb_range =
                        (f32::from(track_bounds.size.height) - metrics.thumb_height).max(0.0);
                    if thumb_range > 0.0 {
                        let thumb_top =
                            (f32::from(event.position.y - track_bounds.top() - next.drag_offset_y))
                                .clamp(0.0, thumb_range);
                        let target = metrics.max_offset * (thumb_top / thumb_range);
                        let offset = scroll_handle.offset();
                        if (f32::from(offset.y) + target).abs() >= 1.0 {
                            scroll_handle.set_offset(point(offset.x, px(-target)));
                            changed = true;
                        }
                    }
                }

                state.set(next);
                if changed {
                    cx.notify(view_id);
                }
            }
        });

        window.on_mouse_event({
            let state = state.clone();
            move |_: &MouseUpEvent, phase, _, cx| {
                if !phase.bubble() || !state.get().dragging {
                    return;
                }
                let mut next = state.get();
                next.dragging = false;
                state.set(next);
                cx.notify(view_id);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_when_content_does_not_overflow() {
        assert!(vertical_scrollbar_metrics(200.0, 200.0, 0.0, 0.0).is_none());
    }

    #[test]
    fn clamps_small_thumb_to_minimum_height() {
        let metrics = vertical_scrollbar_metrics(200.0, 200.0, 1800.0, 0.0).unwrap();

        assert_eq!(metrics.thumb_height, MIN_THUMB_HEIGHT_PX);
        assert_eq!(metrics.thumb_top, 0.0);
    }

    #[test]
    fn maps_scroll_offset_to_thumb_range() {
        let metrics = vertical_scrollbar_metrics(200.0, 200.0, 200.0, 100.0).unwrap();

        assert_eq!(metrics.thumb_height, 100.0);
        assert_eq!(metrics.thumb_top, 50.0);
        assert_eq!(metrics.max_offset, 200.0);
    }

    #[test]
    fn right_inset_moves_track_away_from_window_resize_edge() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(200.0), px(100.0)));
        let track = vertical_scrollbar_track_bounds(bounds, px(8.0), px(4.0));

        assert_eq!(track.right(), px(196.0));
        assert_eq!(track.top(), px(8.0));
        assert_eq!(track.size.width, SCROLLBAR_WIDTH);
        assert_eq!(track.size.height, px(84.0));
    }
}
