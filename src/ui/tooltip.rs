use gpui::{
    AnyView, App, AppContext as _, Context, IntoElement, ParentElement, Render, SharedString,
    Styled, Window, div, px,
};

use crate::theme;

pub(crate) fn text_tooltip(text: impl Into<SharedString>, cx: &mut App) -> AnyView {
    let text = text.into();
    cx.new(|_| TextTooltip { text }).into()
}

struct TextTooltip {
    text: SharedString,
}

impl Render for TextTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);

        div()
            .max_w(px(420.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.input_border)
            .bg(theme.dialog_background)
            .shadow_lg()
            .px_2()
            .py_1()
            .text_sm()
            .whitespace_normal()
            .text_color(theme.foreground)
            .child(self.text.clone())
    }
}
