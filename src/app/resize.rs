use gpui::{
    Bounds, CursorStyle, Edges, HitboxBehavior, IntoElement, MouseButton, MouseDownEvent, Pixels,
    ResizeEdge, Styled, canvas, point, px,
};

pub(super) const WINDOW_RESIZE_OUTSET: Pixels = px(8.0);
const WINDOW_RESIZE_CORNER_LENGTH: Pixels = px(12.0);

pub(super) fn resize_handles(edges: Edges<Pixels>) -> impl IntoElement {
    canvas(
        move |bounds, window, _| {
            resize_regions(bounds, edges)
                .into_iter()
                .map(|(edge, bounds)| {
                    (
                        edge,
                        window.insert_hitbox(bounds, HitboxBehavior::BlockMouse),
                    )
                })
                .collect::<Vec<_>>()
        },
        |_, handles, window, _| {
            for (edge, hitbox) in handles {
                let cursor = match edge {
                    ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
                    ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
                    ResizeEdge::TopLeft | ResizeEdge::BottomRight => {
                        CursorStyle::ResizeUpLeftDownRight
                    }
                    ResizeEdge::TopRight | ResizeEdge::BottomLeft => {
                        CursorStyle::ResizeUpRightDownLeft
                    }
                };
                window.set_cursor_style(cursor, &hitbox);
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    if phase.bubble()
                        && event.button == MouseButton::Left
                        && hitbox.is_hovered(window)
                    {
                        cx.stop_propagation();
                        window.start_window_resize(edge);
                    }
                });
            }
        },
    )
    .absolute()
    .inset_0()
    .size_full()
}

fn resize_regions(
    bounds: Bounds<Pixels>,
    edges: Edges<Pixels>,
) -> Vec<(ResizeEdge, Bounds<Pixels>)> {
    let content = Bounds::from_corners(
        point(bounds.left() + edges.left, bounds.top() + edges.top),
        point(bounds.right() - edges.right, bounds.bottom() - edges.bottom),
    );
    let corner_end = |edge, start: Pixels, center| {
        if edge > px(0.0) {
            (start + WINDOW_RESIZE_CORNER_LENGTH).min(center)
        } else {
            start
        }
    };
    let corner_start = |edge, end: Pixels, center| {
        if edge > px(0.0) {
            (end - WINDOW_RESIZE_CORNER_LENGTH).max(center)
        } else {
            end
        }
    };
    let left = corner_end(edges.left, content.left(), content.center().x);
    let right = corner_start(edges.right, content.right(), content.center().x);
    let top = corner_end(edges.top, content.top(), content.center().y);
    let bottom = corner_start(edges.bottom, content.bottom(), content.center().y);

    // Split each band into a straight edge and two corner segments. The corner
    // segments form L shapes around the content, leaving every interior pixel
    // available to titlebar buttons, scrollbars and other page controls.
    let mut regions = Vec::with_capacity(12);
    let mut push_region = |edge, left, top, right, bottom| {
        let bounds = Bounds::from_corners(point(left, top), point(right, bottom));
        if !bounds.is_empty() {
            regions.push((edge, bounds));
        }
    };
    for (y_start, y_end, resize_edges) in [
        (
            bounds.top(),
            content.top(),
            [ResizeEdge::TopLeft, ResizeEdge::Top, ResizeEdge::TopRight],
        ),
        (
            content.bottom(),
            bounds.bottom(),
            [
                ResizeEdge::BottomLeft,
                ResizeEdge::Bottom,
                ResizeEdge::BottomRight,
            ],
        ),
    ] {
        for ((x_start, x_end), edge) in [
            (bounds.left(), left),
            (left, right),
            (right, bounds.right()),
        ]
        .into_iter()
        .zip(resize_edges)
        {
            push_region(edge, x_start, y_start, x_end, y_end);
        }
    }
    for (x_start, x_end, resize_edges) in [
        (
            bounds.left(),
            content.left(),
            [
                ResizeEdge::TopLeft,
                ResizeEdge::Left,
                ResizeEdge::BottomLeft,
            ],
        ),
        (
            content.right(),
            bounds.right(),
            [
                ResizeEdge::TopRight,
                ResizeEdge::Right,
                ResizeEdge::BottomRight,
            ],
        ),
    ] {
        for ((y_start, y_end), edge) in [
            (content.top(), top),
            (top, bottom),
            (bottom, content.bottom()),
        ]
        .into_iter()
        .zip(resize_edges)
        {
            push_region(edge, x_start, y_start, x_end, y_end);
        }
    }
    regions
}

#[cfg(test)]
mod tests;
