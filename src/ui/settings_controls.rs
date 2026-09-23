//! Compact dropdowns and editable steppers following Zed's settings controls.

use std::{cell::Cell, rc::Rc};

use gpui::{
    App, AppContext as _, Bounds, Context, Entity, FocusHandle, InteractiveElement, IntoElement,
    Modifiers, MouseButton, ParentElement, RenderOnce, StatefulInteractiveElement, Styled,
    Subscription, Window, anchored, canvas, deferred, div, point, prelude::FluentBuilder, px, svg,
};

use crate::{player::TrackLanguage, theme};

use super::{
    editor::{Editor, EditorEvent},
    tooltip::text_tooltip,
};

pub(crate) const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;

pub(crate) fn settings_sidebar(
    bottom_left_radius: gpui::Pixels,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);
    div()
        .id("settings-sidebar")
        .debug_selector(|| "settings-sidebar".into())
        .flex()
        .flex_col()
        .flex_shrink_0()
        .w(px(226.0))
        .p_2p5()
        .gap_4()
        .border_r_1()
        .border_color(theme.title_bar_border)
        .bg(theme.panel_background)
        .rounded_bl(bottom_left_radius)
        .overflow_hidden()
}

pub(crate) fn settings_category_button(
    title: &'static str,
    selected: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);
    div()
        .id(title)
        .role(gpui::Role::Button)
        .aria_label(title)
        .debug_selector(move || format!("settings-category-{title}"))
        .flex()
        .items_center()
        .h(px(28.0))
        .px_2()
        .gap_2()
        .rounded(px(4.0))
        .cursor_pointer()
        .text_sm()
        .text_color(if selected {
            theme.accent_text
        } else {
            theme.muted_foreground
        })
        .when(selected, |this| {
            this.bg(theme.element_selected)
                .font_weight(gpui::FontWeight::MEDIUM)
        })
        .hover(|style| {
            style
                .bg(if selected {
                    theme.element_selected_hover
                } else {
                    theme.secondary_hover
                })
                .text_color(if selected {
                    theme.accent_text
                } else {
                    theme.foreground
                })
        })
        .child(title)
}

pub(crate) fn toggle_switch(
    label: &'static str,
    selected: bool,
    on_toggle: impl Fn(&mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);
    div()
        .id(label)
        .role(gpui::Role::Switch)
        .aria_label(label)
        .aria_toggled(if selected {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .debug_selector(move || format!("settings-toggle-{label}"))
        .group("settings-toggle")
        .flex()
        .items_center()
        .p(px(3.0))
        .cursor_pointer()
        .child(
            div()
                .id("switch-track")
                .flex()
                .items_center()
                .w(px(32.0))
                .h(px(20.0))
                .px(px(2.0))
                .rounded_full()
                .border_1()
                .border_color(if selected {
                    theme.input_border_focused
                } else {
                    theme.input_border
                })
                .bg(if selected {
                    theme.accent
                } else {
                    theme.input_background
                })
                .group_hover("settings-toggle", |style| {
                    style
                        .bg(if selected {
                            theme.accent_hover
                        } else {
                            theme.secondary_hover
                        })
                        .border_color(theme.accent)
                })
                .when(selected, |this| this.justify_end())
                .child(div().size(px(12.0)).rounded_full().bg(if selected {
                    theme.accent_foreground
                } else {
                    theme.muted_foreground
                })),
        )
        .on_click(move |_, _, cx| on_toggle(cx))
}

pub(crate) fn disk_cache_capacity_input<T: 'static>(
    bytes: u64,
    on_change: impl Fn(&mut T, u64, &mut Context<T>) + 'static,
    cx: &mut Context<T>,
) -> Entity<Editor> {
    let input = cx.new(|cx| {
        Editor::new("磁盘缓存上限（GiB）", cx)
            .default_value((bytes / BYTES_PER_GIB).to_string())
            .borderless()
            .compact()
            .height(px(26.0))
            .centered()
            .digits_only()
            .max_chars(12)
    });
    cx.subscribe(&input, move |this, input, event, cx| {
        if matches!(event, EditorEvent::Changed)
            && let Some(bytes) = input
                .read(cx)
                .value()
                .trim()
                .parse::<u64>()
                .ok()
                .and_then(|value| value.checked_mul(BYTES_PER_GIB))
                .filter(|value| *value > 0)
        {
            on_change(this, bytes, cx);
        }
    })
    .detach();
    input
}

pub(crate) fn disk_cache_capacity_control(input: Entity<Editor>, bytes: u64) -> NumberControl {
    NumberControl::new(
        ("disk-cache", "磁盘缓存上限"),
        input,
        "GiB",
        NumberRange::integer(1, u64::MAX / BYTES_PER_GIB),
        (bytes / BYTES_PER_GIB) as f64,
    )
}

pub(crate) fn track_language_selector(
    header: (&'static str, &'static str),
    state: Entity<DropdownState>,
    selected: TrackLanguage,
    on_select: impl Fn(TrackLanguage, &mut App) + 'static,
) -> impl IntoElement {
    selector_row(
        header,
        state,
        TrackLanguage::ALL.map(|language| (language.id(), language.label(), language)),
        selected,
        on_select,
    )
}

pub(crate) struct DropdownState {
    open: Option<&'static str>,
    suppressed_click: Option<&'static str>,
    active: usize,
    focus: FocusHandle,
    blur_subscription: Option<Subscription>,
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
        }
    }

    pub(crate) fn close(&mut self, cx: &mut Context<Self>) {
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
        let state = cx.weak_entity();
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |window, cx| {
                state
                    .update(cx, |state, cx| {
                        if state.open == Some(id) {
                            state.focus.focus(window, cx);
                        }
                    })
                    .ok();
            });
        });
        cx.notify();
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
    SettingsDropdown::new(
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
pub(crate) struct SettingsDropdown {
    id: &'static str,
    label: &'static str,
    options: Vec<(&'static str, &'static str)>,
    selected: usize,
    state: Entity<DropdownState>,
    on_select: SelectCallback,
}

impl SettingsDropdown {
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

impl RenderOnce for SettingsDropdown {
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
            .rounded(px(6.0))
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
                    .flex()
                    .flex_col()
                    .w(px(200.0))
                    .p_1()
                    .rounded(px(8.0))
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
                                .rounded(px(4.0))
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

#[derive(Clone, Copy)]
pub(crate) struct NumberRange {
    min: f64,
    max: f64,
    fractional: bool,
}

impl NumberRange {
    pub(crate) const fn integer(min: u64, max: u64) -> Self {
        Self {
            min: min as f64,
            max: max as f64,
            fractional: false,
        }
    }

    pub(crate) const fn seconds() -> Self {
        // Match the editor's 16-character limit, including three decimal places.
        Self {
            min: 0.0,
            max: 999_999_999_999.999,
            fractional: true,
        }
    }

    fn value(self, text: &str, fallback: f64) -> f64 {
        text.trim()
            .parse::<f64>()
            .ok()
            .filter(|value| {
                value.is_finite() && *value >= 0.0 && (self.fractional || value.fract() == 0.0)
            })
            .unwrap_or(fallback)
    }

    fn stepped(self, text: &str, fallback: f64, increment: bool, modifiers: Modifiers) -> String {
        let step = if modifiers.shift {
            10.0
        } else if modifiers.alt && self.fractional {
            0.1
        } else {
            1.0
        };
        let current = self.value(text, fallback);
        let next = (current + if increment { step } else { -step }).clamp(self.min, self.max);
        if self.fractional {
            format!("{next:.3}")
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_owned()
        } else {
            format!("{next:.0}")
        }
    }
}

#[derive(IntoElement)]
pub(crate) struct NumberControl {
    id: &'static str,
    label: &'static str,
    input: Entity<Editor>,
    unit: &'static str,
    range: NumberRange,
    fallback: f64,
}

impl NumberControl {
    pub(crate) fn new(
        id: (&'static str, &'static str),
        input: Entity<Editor>,
        unit: &'static str,
        range: NumberRange,
        fallback: f64,
    ) -> Self {
        Self {
            id: id.0,
            label: id.1,
            input,
            unit,
            range,
            fallback,
        }
    }
}

impl RenderOnce for NumberControl {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = theme::get(cx);
        let value = self
            .range
            .value(self.input.read(cx).value().as_ref(), self.fallback);
        let focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        // The three joined segments share one focus color so the input's
        // vertical separators and the outer border stay continuous.
        let border = if focused {
            theme.input_border_focused
        } else {
            theme.input_border
        };
        let step_hint = if self.range.fractional {
            "每次加减 1；Shift 加减 10；Alt 加减 0.1"
        } else {
            "每次加减 1；Shift 加减 10"
        };
        let button = |increment| {
            let input = self.input.clone();
            let disabled = if increment {
                value >= self.range.max
            } else {
                value <= self.range.min
            };
            let suffix = if increment { "increment" } else { "decrement" };
            div()
                .id((gpui::ElementId::from(self.id), suffix))
                .debug_selector(move || format!("{}-{suffix}", self.id))
                .role(gpui::Role::Button)
                .aria_label(format!(
                    "{}{}",
                    if increment { "增加" } else { "减少" },
                    self.label
                ))
                .aria_description(step_hint)
                .tooltip(move |_, cx| text_tooltip(step_hint, cx))
                .flex()
                .items_center()
                .justify_center()
                .w(px(28.0))
                .h(px(28.0))
                .flex_shrink_0()
                .border_1()
                .when(increment, |this| {
                    this.rounded_tr(px(6.0)).rounded_br(px(6.0))
                })
                .when(!increment, |this| {
                    this.rounded_tl(px(6.0)).rounded_bl(px(6.0))
                })
                .border_color(border)
                .bg(theme.input_background)
                .child(
                    svg()
                        .path(if increment {
                            "icons/plus.svg"
                        } else {
                            "icons/minus.svg"
                        })
                        .size(px(14.0))
                        .text_color(theme.foreground)
                        .when(disabled, |this| this.opacity(0.35)),
                )
                .when(!disabled, |this| {
                    this.cursor_pointer()
                        .hover(|style| style.bg(theme.secondary_hover))
                        .on_click(move |event, window, cx| {
                            input.update(cx, |input, cx| {
                                let next = self.range.stepped(
                                    input.value().as_ref(),
                                    self.fallback,
                                    increment,
                                    event.modifiers(),
                                );
                                input.set_value(next, cx);
                                input.focus_handle(cx).focus(window, cx);
                            });
                            cx.stop_propagation();
                        })
                })
        };

        div()
            .flex()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .child(
                div()
                    .id(self.id)
                    .debug_selector(move || self.id.into())
                    .role(gpui::Role::SpinButton)
                    .aria_label(self.label)
                    .aria_numeric_value(value)
                    .aria_min_numeric_value(self.range.min)
                    .aria_max_numeric_value(self.range.max)
                    .aria_numeric_value_step(1.0)
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .h(px(28.0))
                    .child(button(false))
                    .child(
                        div()
                            .id((gpui::ElementId::from(self.id), "input"))
                            .debug_selector(move || format!("{}-input", self.id))
                            .flex()
                            .items_center()
                            .w(px(64.0))
                            .h(px(28.0))
                            .border_y_1()
                            .border_color(border)
                            .bg(theme.input_background)
                            .overflow_hidden()
                            .child(self.input.clone()),
                    )
                    .child(button(true)),
            )
            .child(
                div()
                    .w(px(28.0))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(self.unit),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stepper_clamps_limits_and_recovers_invalid_input_from_saved_value() {
        let range = NumberRange::integer(1, 64);
        assert_eq!(range.stepped("0", 10.0, true, Modifiers::default()), "1");
        assert_eq!(range.stepped("1", 10.0, false, Modifiers::default()), "1");
        assert_eq!(range.stepped("64", 10.0, true, Modifiers::default()), "64");
        assert_eq!(range.stepped("999", 10.0, true, Modifiers::default()), "64");
        for invalid in ["", "NaN", "inf", "-1", "1.5", "abc"] {
            assert_eq!(
                range.stepped(invalid, 10.0, true, Modifiers::default()),
                "11"
            );
        }
        let max = NumberRange::integer(0, 999_999_999_999);
        assert_eq!(
            max.stepped("999999999999", 0.0, true, Modifiers::default()),
            "999999999999"
        );
    }

    #[test]
    fn fractional_steps_preserve_precision_and_shift_overrides_alt() {
        let range = NumberRange::seconds();
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(range.stepped("0.2", 1.0, true, alt), "0.3");
        assert_eq!(range.stepped("0.025", 1.0, true, alt), "0.125");
        assert_eq!(range.stepped("0.025", 1.0, false, alt), "0");
        assert_eq!(
            range.stepped("1.25", 1.0, true, Modifiers { shift: true, ..alt }),
            "11.25"
        );
        let max = range.stepped("999999999999.999", 1.0, true, Modifiers::default());
        assert!(max.len() <= 16);
        assert!(max.parse::<f64>().unwrap().is_finite());
    }
}
