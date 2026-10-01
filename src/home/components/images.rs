//! Home images presentation components.
use super::*;

pub(in crate::home) fn cover_img(path: Arc<Path>, width: f32, height: f32) -> impl IntoElement {
    let source = CoverImageRequest {
        path,
        width: width as u32,
        height: height as u32,
    };

    img(move |window: &mut Window, cx: &mut App| window.use_asset::<CoverImageAsset>(&source, cx))
        .w(px(width))
        .h(px(height))
        .rounded(radius::CARD)
}

pub(super) fn cover_image_progress_bar<T>(
    played_fraction: f32,
    cx: &Context<T>,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let track_color = theme.background.opacity(0.72);
    let progress_color = theme.input_border_focused;

    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let radius = radius::CARD.to_pixels(window.rem_size());
            let height = px(IMAGE_PROGRESS_BAR_HEIGHT_PX).min(bounds.size.height);
            let track_bounds = Bounds::new(
                point(bounds.left(), bounds.bottom() - height),
                size(bounds.size.width, height),
            );
            let progress_bounds = Bounds::new(
                track_bounds.origin,
                size(bounds.size.width * played_fraction.clamp(0.0, 1.0), height),
            );

            // GPUI overflow masks are rectangular. Reveal only the bottom strip
            // of full-cover rounded quads so both colors follow the cover corners.
            window.with_content_mask(
                Some(ContentMask {
                    bounds: track_bounds,
                }),
                |window| {
                    window.paint_quad(fill(bounds, track_color).corner_radii(radius));
                },
            );
            window.with_content_mask(
                Some(ContentMask {
                    bounds: progress_bounds,
                }),
                |window| {
                    window.paint_quad(fill(bounds, progress_color).corner_radii(radius));
                },
            );
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

pub(super) fn image_progress_bar<T>(
    image_width: f32,
    played_fraction: f32,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);
    let track_width = (image_width - IMAGE_PROGRESS_BAR_HORIZONTAL_INSET_PX * 2.0).max(0.0);

    div()
        .absolute()
        .left(px(IMAGE_PROGRESS_BAR_HORIZONTAL_INSET_PX))
        .right(px(IMAGE_PROGRESS_BAR_HORIZONTAL_INSET_PX))
        .bottom_0()
        .h(px(IMAGE_PROGRESS_BAR_HEIGHT_PX))
        .rounded_full()
        .overflow_hidden()
        .bg(theme.background.opacity(0.72))
        .child(
            div()
                .h_full()
                .w(px(track_width * played_fraction.clamp(0.0, 1.0)))
                .rounded_full()
                .bg(theme.input_border_focused),
        )
}
