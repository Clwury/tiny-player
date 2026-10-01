//! Reusable bounded numeric input and stepper.
use super::{editor::Editor, tooltip::text_tooltip};
use crate::{theme, ui::radius};
use gpui::prelude::FluentBuilder;
use gpui::{
    App, Entity, InteractiveElement, IntoElement, Modifiers, ParentElement, RenderOnce,
    StatefulInteractiveElement, Styled, Window, div, px, svg,
};

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
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = theme::get(cx);
        let value = self
            .range
            .value(self.input.read(cx).value().as_ref(), self.fallback);
        // All three segments share the text editor's neutral border and surface.
        let border = theme.window_border;
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
                    this.rounded_tr(radius::INPUT).rounded_br(radius::INPUT)
                })
                .when(!increment, |this| {
                    this.rounded_tl(radius::INPUT).rounded_bl(radius::INPUT)
                })
                .border_color(border)
                .bg(theme.editor_background)
                .cursor_default()
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
                            .bg(theme.editor_background)
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
