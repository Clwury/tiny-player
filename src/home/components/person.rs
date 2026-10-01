//! Home person presentation components.
use super::*;

pub(in crate::home) fn person_card<T>(
    view: PersonCardVm,
    image_path: Option<Arc<Path>>,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .flex_none()
        .flex_col()
        .items_center()
        .gap_2()
        .rounded(radius::CARD)
        .p(px(DETAIL_PERSON_CARD_PADDING_PX))
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(person_card_image(image_path, cx))
        .child(
            div()
                .w(px(DETAIL_PERSON_CARD_WIDTH_PX))
                .flex()
                .flex_col()
                .gap_1()
                .text_center()
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child(view.name),
                )
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(view.role),
                )
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(view.kind),
                ),
        )
}

fn person_card_image<T>(image_path: Option<Arc<Path>>, cx: &Context<T>) -> impl IntoElement {
    let theme = theme::get(cx);
    let has_image = image_path.is_some();

    div()
        .relative()
        .w(px(DETAIL_PERSON_CARD_IMAGE_WIDTH_PX))
        .h(px(DETAIL_PERSON_CARD_IMAGE_HEIGHT_PX))
        .overflow_hidden()
        .rounded(radius::CARD)
        .bg(theme.input_background)
        .when_some(image_path, |this, path| {
            this.child(cover_img(
                path,
                DETAIL_PERSON_CARD_IMAGE_WIDTH_PX,
                DETAIL_PERSON_CARD_IMAGE_HEIGHT_PX,
            ))
        })
        .when(!has_image, |this| {
            this.flex().items_center().justify_center().child(
                svg()
                    .path("icons/circle-user-round.svg")
                    .size(px(32.0))
                    .text_color(theme.muted_foreground),
            )
        })
}
