use gpui::{CursorStyle, Div, InteractiveElement, MouseButton, ResizeEdge, Styled, div, px};

pub(super) fn resize_handles() -> Vec<Div> {
    let edge_size = px(6.0);
    let corner_size = px(12.0);

    vec![
        resize_handle(ResizeEdge::Top, CursorStyle::ResizeUpDown)
            .top_0()
            .left(corner_size)
            .right(corner_size)
            .h(edge_size),
        resize_handle(ResizeEdge::Right, CursorStyle::ResizeLeftRight)
            .top(corner_size)
            .right_0()
            .bottom(corner_size)
            .w(edge_size),
        resize_handle(ResizeEdge::Bottom, CursorStyle::ResizeUpDown)
            .left(corner_size)
            .right(corner_size)
            .bottom_0()
            .h(edge_size),
        resize_handle(ResizeEdge::Left, CursorStyle::ResizeLeftRight)
            .top(corner_size)
            .left_0()
            .bottom(corner_size)
            .w(edge_size),
        resize_handle(ResizeEdge::TopLeft, CursorStyle::ResizeUpLeftDownRight)
            .top_0()
            .left_0()
            .size(corner_size),
        resize_handle(ResizeEdge::TopRight, CursorStyle::ResizeUpRightDownLeft)
            .top_0()
            .right_0()
            .size(corner_size),
        resize_handle(ResizeEdge::BottomRight, CursorStyle::ResizeUpLeftDownRight)
            .right_0()
            .bottom_0()
            .size(corner_size),
        resize_handle(ResizeEdge::BottomLeft, CursorStyle::ResizeUpRightDownLeft)
            .left_0()
            .bottom_0()
            .size(corner_size),
    ]
}

fn resize_handle(edge: ResizeEdge, cursor: CursorStyle) -> Div {
    div().absolute().flex_none().cursor(cursor).on_mouse_down(
        MouseButton::Left,
        move |_, window, cx| {
            cx.stop_propagation();
            window.start_window_resize(edge);
        },
    )
}
