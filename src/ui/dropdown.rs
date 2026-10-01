//! Reusable dropdown interaction, deferred focus and option rendering.
use crate::effects::{RequestScope, RequestSlot, RequestToken, WorkspaceIdentity};
use crate::{theme, ui::radius};
use gpui::prelude::FluentBuilder;
use gpui::{
    App, Bounds, Context, Entity, FocusHandle, InteractiveElement, IntoElement, MouseButton,
    ParentElement, RenderOnce, StatefulInteractiveElement, Styled, Subscription, Window, anchored,
    canvas, deferred, div, point, px, svg,
};
use std::{cell::Cell, rc::Rc};

pub(crate) struct DropdownState {
    open: Option<&'static str>,
    suppressed_click: Option<&'static str>,
    active: usize,
    focus: FocusHandle,
    blur_subscription: Option<Subscription>,
    // Owned by this dropdown entity. Each show replaces the deferred focus;
    // close invalidates it and release drops the slot. GPUI frame callbacks
    // cannot be removed, so weak-entity + token checks cancel delivery. This
    // account-independent UI operation has no business error notification.
    pending_focus: RequestSlot,
}

impl DropdownState {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        Self {
            open: None,
            suppressed_click: None,
            active: 0,
            focus,
            blur_subscription: None,
            pending_focus: RequestSlot::new(
                RequestScope::DropdownFocus,
                WorkspaceIdentity::default(),
            ),
        }
    }

    pub(crate) fn close(&mut self, cx: &mut Context<Self>) {
        self.pending_focus.invalidate();
        if self.open.take().is_some() {
            cx.notify();
        }
    }

    fn show(
        &mut self,
        id: &'static str,
        selected: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open = Some(id);
        self.active = selected;
        // Like Zed's PopoverMenu, focus after deferred menu drawing has joined
        // the dispatch tree. A menu dismissed in the meantime must stay closed.
        let token = self.pending_focus.issue();
        let state = cx.weak_entity();
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| {
                state
                    .update(cx, |state, cx| {
                        state.finish_focus(id, &token, window, cx);
                    })
                    .ok();
            });
        });
        cx.notify();
    }

    fn finish_focus(
        &mut self,
        id: &'static str,
        token: &RequestToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.open == Some(id) && self.pending_focus.commit(token) {
            self.focus.focus(window, cx);
        }
    }
}

type SelectCallback = Rc<dyn Fn(usize, &mut App)>;

pub(crate) fn selector_row<T: Copy + PartialEq + 'static>(
    header: (&'static str, &'static str),
    state: Entity<DropdownState>,
    options: impl IntoIterator<Item = (&'static str, &'static str, T)>,
    selected: T,
    on_select: impl Fn(T, &mut App) + 'static,
) -> impl IntoElement {
    let options: Vec<_> = options.into_iter().collect();
    let selected_index = options
        .iter()
        .position(|(_, _, value)| *value == selected)
        .expect("settings dropdown must include its current value");
    Dropdown::new(
        header.0,
        header.1,
        options
            .iter()
            .map(|(id, label, _)| (*id, *label))
            .collect::<Vec<_>>(),
        selected_index,
        state,
        move |index, cx| on_select(options[index].2, cx),
    )
}

#[derive(IntoElement)]
pub(crate) struct Dropdown {
    id: &'static str,
    label: &'static str,
    options: Vec<(&'static str, &'static str)>,
    selected: usize,
    state: Entity<DropdownState>,
    on_select: SelectCallback,
}

impl Dropdown {
    pub(crate) fn new(
        id: &'static str,
        label: &'static str,
        options: impl IntoIterator<Item = (&'static str, &'static str)>,
        selected: usize,
        state: Entity<DropdownState>,
        on_select: impl Fn(usize, &mut App) + 'static,
    ) -> Self {
        let options: Vec<_> = options.into_iter().collect();
        assert!(
            selected < options.len(),
            "dropdown must have a valid selection"
        );
        Self {
            id,
            label,
            options,
            selected,
            state,
            on_select: Rc::new(on_select),
        }
    }
}

impl RenderOnce for Dropdown {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        self.state.update(cx, |state, cx| {
            if state.blur_subscription.is_none() {
                state.blur_subscription =
                    Some(cx.on_blur(&state.focus, window, |this, _, cx| this.close(cx)));
            }
        });
        let theme = theme::get(cx);
        let border = theme.input_border;
        let focused_border = theme.input_border_focused;
        let background = theme.input_background;
        let hover = theme.secondary_hover;
        let selected_background = theme.element_selected;
        let selected_hover = theme.element_selected_hover;
        let accent = theme.accent_text;
        let foreground = theme.foreground;
        let muted = theme.muted_foreground;
        let menu_background = theme.dialog_background;
        let focus = window
            .use_keyed_state(self.id, cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let trigger_bounds = Rc::new(Cell::new(Bounds::default()));
        let open = self.state.read(cx).open == Some(self.id);
        let active = self.state.read(cx).active;
        let id = self.id;
        let selected = self.selected;
        let option_count = self.options.len();
        let state = self.state.clone();
        let key_state = self.state.clone();
        let mouse_state = self.state.clone();
        let mouse_focus = focus.clone();
        let bounds = trigger_bounds.clone();
        let trigger = div()
            .id(id)
            .debug_selector(move || id.into())
            .role(gpui::Role::ComboBox)
            .aria_label(self.label)
            .aria_value(self.options[selected].1)
            .aria_expanded(open)
            .track_focus(&focus)
            .relative()
            .flex()
            .items_center()
            .justify_between()
            .gap_1()
            .h(px(28.0))
            .px_2()
            .rounded(radius::CONTROL)
            .border_1()
            .border_color(if open || focus.is_focused(window) {
                focused_border
            } else {
                border
            })
            .bg(background)
            .cursor_pointer()
            .text_sm()
            .text_color(foreground)
            .hover(move |style| style.bg(hover))
            .child(self.options[selected].1)
            .child(
                svg()
                    .path("icons/chevron-up-down.svg")
                    .size(px(12.0))
                    .text_color(muted),
            )
            .child(
                canvas(
                    move |bounds, _, _| trigger_bounds.set(bounds),
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                mouse_state.update(cx, |state, cx| {
                    state.suppressed_click = None;
                    if state.open == Some(id) {
                        state.close(cx);
                        state.suppressed_click = Some(id);
                        mouse_focus.focus(window, cx);
                        cx.stop_propagation();
                    }
                });
            })
            .on_click(move |event, window, cx| {
                state.update(cx, |state, cx| {
                    // Mouse-down dismisses first. Consume the matching release
                    // even if a redraw happened between the two events.
                    if state.suppressed_click.take() == Some(id) && event.mouse_position().is_some()
                    {
                        return;
                    }
                    if state.open == Some(id) {
                        state.close(cx);
                    } else {
                        state.show(id, selected, window, cx);
                    }
                });
            })
            .on_key_down(move |event, window, cx| {
                if matches!(event.keystroke.key.as_str(), "down" | "up") {
                    key_state.update(cx, |state, cx| state.show(id, selected, window, cx));
                    cx.stop_propagation();
                }
            });

        div()
            .relative()
            .flex()
            .flex_shrink_0()
            .child(trigger)
            .when(open, |this| {
                let key_state = self.state.clone();
                let outside_state = self.state.clone();
                let key_focus = focus.clone();
                let on_select = self.on_select.clone();
                let outside_focus = focus.clone();
                let menu = div()
                    .id((gpui::ElementId::from(id), "menu"))
                    .debug_selector(move || format!("{id}-menu"))
                    .role(gpui::Role::ListBox)
                    .aria_label(self.label)
                    .track_focus(&self.state.read(cx).focus)
                    .occlude()
                    .cursor_default()
                    .flex()
                    .flex_col()
                    .w(px(200.0))
                    .p_1()
                    .rounded(radius::SURFACE)
                    .border_1()
                    .border_color(border)
                    .bg(menu_background)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .on_mouse_down_out(move |event, window, cx| {
                        // The trigger handles its own second click to close the menu.
                        if !bounds.get().contains(&event.position) {
                            outside_state.update(cx, |state, cx| {
                                state.close(cx);
                                if state.focus.is_focused(window) {
                                    outside_focus.focus(window, cx);
                                }
                            });
                        }
                    })
                    .on_key_down(move |event, window, cx| {
                        match event.keystroke.key.as_str() {
                            "up" | "down" | "home" | "end" => {
                                key_state.update(cx, |state, cx| {
                                    state.active = match event.keystroke.key.as_str() {
                                        "up" => (state.active + option_count - 1) % option_count,
                                        "down" => (state.active + 1) % option_count,
                                        "home" => 0,
                                        _ => option_count - 1,
                                    };
                                    cx.notify();
                                });
                            }
                            "enter" | "space" => {
                                on_select(key_state.read(cx).active, cx);
                                key_state.update(cx, |state, cx| state.close(cx));
                                key_focus.focus(window, cx);
                            }
                            "escape" | "tab" => {
                                key_state.update(cx, |state, cx| state.close(cx));
                                key_focus.focus(window, cx);
                                if event.keystroke.key == "tab" {
                                    if event.keystroke.modifiers.shift {
                                        window.focus_prev(cx);
                                    } else {
                                        window.focus_next(cx);
                                    }
                                }
                            }
                            _ => return,
                        }
                        cx.stop_propagation();
                    })
                    .children(self.options.into_iter().enumerate().map(
                        |(index, (option_id, label))| {
                            let state = self.state.clone();
                            let on_select = self.on_select.clone();
                            let focus = focus.clone();
                            let hover_state = self.state.clone();
                            div()
                                .id(option_id)
                                .debug_selector(move || option_id.into())
                                .role(gpui::Role::ListBoxOption)
                                .aria_label(label)
                                .aria_selected(index == selected)
                                .flex()
                                .items_center()
                                .justify_between()
                                .h(px(24.0))
                                .px_1p5()
                                .rounded(radius::CONTROL)
                                .text_sm()
                                .text_color(if index == selected {
                                    accent
                                } else {
                                    foreground
                                })
                                .bg(if index == selected {
                                    selected_background
                                } else {
                                    menu_background
                                })
                                .cursor_pointer()
                                .when(index == active && index != selected, |this| this.bg(hover))
                                .hover(move |style| {
                                    style.bg(if index == selected {
                                        selected_hover
                                    } else {
                                        hover
                                    })
                                })
                                .on_hover(move |hovered, _, cx| {
                                    if *hovered {
                                        hover_state.update(cx, |state, cx| {
                                            state.active = index;
                                            cx.notify();
                                        });
                                    }
                                })
                                .child(label)
                                .child(div().size(px(14.0)).when(index == selected, |this| {
                                    this.child(
                                        svg()
                                            .path("icons/check.svg")
                                            .size_full()
                                            .text_color(focused_border),
                                    )
                                }))
                                .on_click(move |_, window, cx| {
                                    on_select(index, cx);
                                    state.update(cx, |state, cx| state.close(cx));
                                    focus.focus(window, cx);
                                    cx.stop_propagation();
                                })
                        },
                    ));
                this.child(
                    // Align the menu's right edge with the trigger so it opens
                    // directly below the control, within the settings column.
                    div()
                        .absolute()
                        .top(gpui::relative(1.0))
                        .left(gpui::relative(1.0))
                        .child(
                            deferred(
                                anchored()
                                    .anchor(gpui::Anchor::TopRight)
                                    .position_mode(gpui::AnchoredPositionMode::Local)
                                    .position(point(px(0.0), px(4.0)))
                                    .snap_to_window_with_margin(px(8.0))
                                    .child(menu),
                            )
                            .with_priority(2),
                        ),
                )
            })
    }
}

#[cfg(test)]
mod tests;
