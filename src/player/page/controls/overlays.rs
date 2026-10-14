use super::*;
use crate::ui::tooltip::text_tooltip;

mod shortcuts;

#[cfg(test)]
mod tests;

const PLAYBACK_OVERLAY_WIDTH_PX: f32 = 1100.0;
const PLAYBACK_OVERLAY_TOP_PX: f32 = 56.0;

impl PlaybackPage {
    pub(in crate::player::page) fn overlay_toolbar_visible(&self) -> bool {
        !self.presentation.episode_list.open && self.presentation.fullscreen.controls_visible
    }

    pub(in crate::player::page) fn toggle_overlay(
        &mut self,
        overlay: PlaybackOverlay,
        cx: &mut Context<Self>,
    ) {
        self.presentation.active_overlay =
            (self.presentation.active_overlay != Some(overlay)).then_some(overlay);
        cx.notify();
    }

    pub(in crate::player::page) fn render_overlay_toolbar(
        &self,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        div()
            .id("playback-overlay-toolbar")
            .debug_selector(|| "playback-overlay-toolbar".into())
            .absolute()
            .right(px(PLAYBACK_BACK_BUTTON_OFFSET_PX))
            .top(px(PLAYBACK_BACK_BUTTON_OFFSET_PX))
            .h(px(PLAYBACK_BACK_BUTTON_SIZE_PX))
            .flex()
            .items_center()
            .gap_1()
            .occlude()
            .on_hover(cx.listener(Self::handle_overlay_toolbar_hover))
            .on_mouse_move(cx.listener(|page, _, _, cx| {
                cx.stop_propagation();
                page.set_overlay_toolbar_hover(true, cx);
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(cx.listener(Self::handle_surface_scroll_wheel))
            .children(self.render_download_speed(cx))
            .child(self.render_overlay_button(
                "playback-keyboard-button",
                "icons/keyboard.svg",
                "快捷键",
                PlaybackOverlay::Shortcuts,
                cx,
            ))
            .child(self.render_overlay_button(
                "playback-info-button",
                "icons/info.svg",
                "播放信息",
                PlaybackOverlay::Info,
                cx,
            ))
    }

    fn render_overlay_button(
        &self,
        id: &'static str,
        icon: &'static str,
        label: &'static str,
        overlay: PlaybackOverlay,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::media_overlay(cx);
        components::playback_control_button(
            id,
            icon,
            px(PLAYBACK_BACK_BUTTON_SIZE_PX),
            px(18.0),
            true,
            cx,
        )
        .aria_label(crate::i18n::tr(label, cx))
        .tooltip(move |_, cx| text_tooltip(crate::i18n::tr(label, cx), cx))
        .when(self.presentation.active_overlay == Some(overlay), |this| {
            this.bg(theme.element_selected)
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |page, _, _, cx| {
                cx.stop_propagation();
                page.toggle_overlay(overlay, cx);
            }),
        )
    }

    fn handle_overlay_toolbar_hover(
        &mut self,
        hovered: &bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_overlay_toolbar_hover(*hovered, cx);
    }

    fn set_overlay_toolbar_hover(&mut self, hovered: bool, cx: &mut Context<Self>) {
        self.presentation.fullscreen.mouse_in_toolbar = hovered;
        if hovered {
            let changed = !self.presentation.fullscreen.controls_visible
                || !self.presentation.fullscreen.cursor_visible;
            self.presentation.fullscreen.controls_visible = true;
            self.presentation.fullscreen.cursor_visible = true;
            self.presentation
                .presentation_timers
                .cancel(PresentationTimer::Controls);
            if changed {
                cx.notify();
            }
        } else {
            self.schedule_fullscreen_controls_hide(cx);
        }
    }

    pub(in crate::player::page) fn render_shortcuts_overlay(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        shortcuts::shortcuts_overlay(
            self.presentation.gamepad_help_first,
            &self.presentation.shortcuts_overlay_scroll_handle,
            window,
            cx,
        )
    }
}

pub(super) fn playback_overlay_container(
    id: &'static str,
    scroll_handle: &gpui::ScrollHandle,
    window: &Window,
) -> gpui::Stateful<gpui::Div> {
    let scroll = scroll_handle.clone();
    div()
        .id(id)
        .debug_selector(move || id.into())
        .absolute()
        .left_4()
        .top(px(if window.is_fullscreen() {
            16.0
        } else {
            PLAYBACK_OVERLAY_TOP_PX
        }))
        .flex()
        .flex_col()
        .w(px(PLAYBACK_OVERLAY_WIDTH_PX))
        .max_w(relative(0.95))
        .max_h(relative(if window.is_fullscreen() { 0.92 } else { 0.82 }))
        .overflow_y_scroll()
        .track_scroll(scroll_handle)
        .p_1()
        .text_sm()
        .line_height(px(20.0))
        .text_color(rgb(0xffffff))
        // Only scrollable content owns the wheel, including at its boundaries.
        // Fitting overlays let the playback surface adjust volume.
        .on_scroll_wheel(move |_, _, cx| {
            if scroll.max_offset().y > px(0.0) {
                cx.stop_propagation();
            }
        })
}
