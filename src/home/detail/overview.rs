use gpui::{
    ClickEvent, Context, CursorStyle, FocusHandle, HitboxBehavior, InteractiveElement, IntoElement,
    MouseButton, ParentElement, ScrollHandle, StatefulInteractiveElement, Styled, StyledText,
    Window, canvas, div, px, svg,
};

use crate::ui::radius;
use crate::{theme, ui::scrollbar::Scrollbar};

use super::{HomeContent, SeriesDetailState};

#[derive(Clone, Debug)]
pub(in crate::home) struct MovieOverviewOverlay {
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    scroll_handle: ScrollHandle,
}

impl HomeContent {
    pub(in crate::home) fn render_movie_overview_preview(
        &self,
        detail: &SeriesDetailState,
        overview: String,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let text = StyledText::new(overview.clone());
        let layout = text.layout().clone();
        let cursor_layout = layout.clone();
        let cursor_text = overview.clone();
        let detail_id = detail.series_id.clone();

        div()
            .id("movie-detail-overview")
            .debug_selector(|| "movie-detail-overview".into())
            .relative()
            .w_full()
            .text_sm()
            .line_height(px(22.0))
            .text_color(theme.muted_foreground)
            .text_ellipsis()
            .line_clamp(3)
            .child(text)
            // Inspect the actual clamped layout after measurement so fonts,
            // CJK wrapping and window resizing all use the same overflow rule.
            .child(
                canvas(
                    move |bounds, window, _| {
                        (cursor_layout.text() != cursor_text)
                            .then(|| window.insert_hitbox(bounds, HitboxBehavior::Normal))
                    },
                    |_, hitbox, window, _| {
                        if let Some(hitbox) = hitbox {
                            window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
            .on_click(cx.listener(move |page, _: &ClickEvent, window, cx| {
                if layout.text() != overview {
                    page.open_movie_overview(&detail_id, window, cx);
                }
            }))
    }

    fn open_movie_overview(
        &mut self,
        detail_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.series_detail.as_mut().filter(|detail| {
            detail.is_movie() && detail.series_id == detail_id && detail.overview_overlay.is_none()
        }) else {
            return;
        };
        let focus = cx.focus_handle();
        let previous_focus = window.focused(cx);
        focus.focus(window, cx);
        detail.open_select = None;
        detail.overview_overlay = Some(MovieOverviewOverlay {
            focus,
            previous_focus,
            scroll_handle: ScrollHandle::new(),
        });
        self.item_context_menu = None;
        cx.notify();
    }

    pub(in crate::home) fn dismiss_movie_overview(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(overlay) = self
            .series_detail
            .as_mut()
            .and_then(|detail| detail.overview_overlay.take())
        else {
            return;
        };
        if overlay.focus.contains_focused(window, cx) {
            if let Some(previous_focus) = overlay.previous_focus {
                previous_focus.focus(window, cx);
            } else {
                window.blur(cx);
            }
        }
        cx.notify();
    }

    pub(in crate::home) fn render_movie_overview_overlay(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<impl IntoElement> {
        let detail = self
            .series_detail
            .as_ref()
            .filter(|detail| detail.is_movie())?;
        let overlay = detail.overview_overlay.as_ref()?;
        // Keep the complete original paragraphs in the expanded view.
        let overview = detail.item.as_ref()?.overview.as_deref()?.trim().to_owned();
        let theme = theme::get(cx);

        Some(
            div()
                .id("movie-overview-overlay")
                .cursor_default()
                .debug_selector(|| "movie-overview-overlay".into())
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .p_6()
                .bg(theme.overlay)
                .occlude()
                .track_focus(&overlay.focus)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .on_click(
                    cx.listener(|page, _, window, cx| page.dismiss_movie_overview(window, cx)),
                )
                .on_key_down(cx.listener(|page, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        page.dismiss_movie_overview(window, cx);
                    }
                    cx.stop_propagation();
                }))
                .child(
                    div()
                        .id("movie-overview-panel")
                        .debug_selector(|| "movie-overview-panel".into())
                        .flex()
                        .flex_col()
                        .w(px(640.0))
                        .max_w_full()
                        .max_h_full()
                        .gap_4()
                        .p_5()
                        .rounded(radius::SURFACE)
                        .border_1()
                        .border_color(theme.input_border)
                        .bg(theme.dialog_background)
                        .text_color(theme.foreground)
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(
                            div()
                                .flex()
                                .flex_none()
                                .items_center()
                                .justify_between()
                                .gap_3()
                                .child(
                                    div()
                                        .text_lg()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child("剧情简介"),
                                )
                                .child(
                                    div()
                                        .id("movie-overview-close")
                                        .debug_selector(|| "movie-overview-close".into())
                                        .flex()
                                        .flex_none()
                                        .size(px(28.0))
                                        .items_center()
                                        .justify_center()
                                        .rounded(radius::CONTROL)
                                        .cursor_pointer()
                                        .hover(move |style| style.bg(theme.secondary_hover))
                                        .child(
                                            svg()
                                                .path("icons/window-close.svg")
                                                .size(px(18.0))
                                                .text_color(theme.foreground),
                                        )
                                        .on_click(cx.listener(|page, _, window, cx| {
                                            cx.stop_propagation();
                                            page.dismiss_movie_overview(window, cx);
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .relative()
                                .min_h_0()
                                .child(
                                    div()
                                        .id("movie-overview-scroll")
                                        .debug_selector(|| "movie-overview-scroll".into())
                                        .min_h_0()
                                        .max_h(px(f32::from(window.bounds().size.height) * 0.6))
                                        .overflow_y_scroll()
                                        .track_scroll(&overlay.scroll_handle)
                                        .pr_3()
                                        .text_sm()
                                        .line_height(px(24.0))
                                        .child(
                                            div()
                                                .debug_selector(|| "movie-overview-text".into())
                                                .child(overview),
                                        ),
                                )
                                .child(
                                    Scrollbar::vertical(&overlay.scroll_handle)
                                        .id("movie-overview-scrollbar"),
                                ),
                        ),
                ),
        )
    }
}

#[cfg(test)]
#[path = "overview_tests.rs"]
mod tests;
