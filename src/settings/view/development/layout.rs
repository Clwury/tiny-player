//! Development settings layout and row rendering.
use super::*;

pub(super) struct SettingItem {
    pub(super) descriptor: SettingDescriptor,
    control: AnyElement,
}

impl SettingItem {
    pub(super) fn new(
        category: SettingsCategory,
        section: &'static str,
        title: &'static str,
        description: &'static str,
        keywords: &'static str,
        control: impl IntoElement,
    ) -> Self {
        Self {
            descriptor: SettingDescriptor {
                category,
                section,
                title,
                description,
                keywords,
            },
            control: control.into_any_element(),
        }
    }

    pub(super) fn render(self, last_in_section: bool, cx: &App) -> impl IntoElement {
        let theme = theme::get(cx);
        div()
            .id(self.descriptor.title)
            .debug_selector(move || self.descriptor.title.into())
            .flex()
            .min_w_0()
            .w_full()
            // Zed SettingsPageItem::render: 16px between rows, 40px after a section.
            .pt_4()
            .when_else(
                last_in_section,
                |this| this.pb_10(),
                |this| {
                    this.pb_4()
                        .border_b_1()
                        .border_color(theme.title_bar_border)
                },
            )
            .gap_4()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .max_w(relative(2.0 / 3.0))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.foreground)
                            .child(self.descriptor.title),
                    )
                    .child(
                        div()
                            .text_xs()
                            .line_height(relative(1.5))
                            .text_color(theme.muted_foreground)
                            .child(self.descriptor.description),
                    ),
            )
            .child(div().flex_shrink_0().child(self.control))
    }
}

impl PlaybackSettingsDialogState {
    pub(super) fn render_content(
        &self,
        dialog: Entity<Self>,
        bottom_left_radius: gpui::Pixels,
        cx: &App,
    ) -> impl IntoElement {
        let dropdown = self.dropdown.clone();
        let query = self.controller.view_model().query;
        let searching = !query.trim().is_empty();
        div()
            .id("playback-settings-panel")
            .debug_selector(|| "playback-settings-panel".into())
            .flex()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            // Keep the panel transparent so the window supplies the rounded background.
            .child(self.render_sidebar(dialog.clone(), searching, bottom_left_radius, cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(
                        div()
                            .id("playback-settings-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .on_scroll_wheel(move |_, _, cx| {
                                dropdown.update(cx, |state, cx| state.close(cx))
                            })
                            .px_8()
                            .child(self.render_settings(dialog, query.trim(), cx)),
                    )
                    .child(
                        div()
                            .id("playback-settings-scrollbar")
                            .debug_selector(|| "playback-settings-scrollbar".into())
                            .absolute()
                            .top_0()
                            .right_0()
                            .bottom_0()
                            .left_0()
                            .child(Scrollbar::vertical(&self.scroll_handle).right_inset(px(4.0))),
                    ),
            )
    }

    fn render_sidebar(
        &self,
        dialog: Entity<Self>,
        searching: bool,
        bottom_left_radius: gpui::Pixels,
        cx: &App,
    ) -> impl IntoElement {
        settings_sidebar(bottom_left_radius, cx)
            .child(self.search.clone())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(SettingsCategory::ALL.into_iter().map(|category| {
                        let selected =
                            !searching && self.controller.view_model().category == category;
                        let dialog = dialog.clone();
                        settings_category_button(category.title(), selected, cx).on_click(
                            move |_, window, cx| {
                                dialog.update(cx, |dialog, cx| {
                                    // Do not leave focus in an editor that disappears with its page.
                                    window.blur(cx);
                                    dialog.dispatch(SettingsIntent::Category(category), cx);
                                    dialog.dropdown.update(cx, |state, cx| state.close(cx));
                                    dialog
                                        .search
                                        .update(cx, |search, cx| search.clear_value(cx));
                                    dialog.scroll_handle.set_offset(point(px(0.0), px(0.0)));
                                    cx.notify();
                                });
                            },
                        )
                    })),
            )
    }

    fn render_settings(&self, dialog: Entity<Self>, query: &str, cx: &App) -> impl IntoElement {
        let theme = theme::get(cx);
        let searching = !query.is_empty();
        let items: Vec<_> = self
            .setting_items(dialog, cx)
            .into_iter()
            .filter(|item| self.controller.view_model().includes(&item.descriptor))
            .collect();
        let mut content = div().flex().flex_col().min_w_0().child(
            div()
                .mt_2()
                .mb_3()
                .text_base()
                .text_color(theme.foreground)
                .child(if searching {
                    "搜索结果"
                } else {
                    self.controller.view_model().category.title()
                }),
        );
        if items.is_empty() {
            return content
                .pt_8()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child("没有找到匹配的设置"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("试试“内存”“预读”或“HTTP”等关键词。"),
                );
        }
        let mut previous_section = None;
        let mut items = items.into_iter().peekable();
        while let Some(item) = items.next() {
            let section = (item.descriptor.category, item.descriptor.section);
            if previous_section != Some(section) {
                content = content.child(
                    div()
                        .pb_1p5()
                        .border_b_1()
                        .border_color(theme.title_bar_border)
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child(if searching {
                            format!(
                                "{} / {}",
                                item.descriptor.category.title(),
                                item.descriptor.section
                            )
                        } else {
                            item.descriptor.section.to_string()
                        }),
                );
                previous_section = Some(section);
            }
            let last_in_section = items
                .peek()
                .is_none_or(|next| (next.descriptor.category, next.descriptor.section) != section);
            content = content.child(item.render(last_in_section, cx));
        }
        content
    }
}
