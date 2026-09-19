//! User-facing preferences. Advanced cache tuning stays in the development dialog.

mod memory_budget;

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, StatefulInteractiveElement, Styled, Window, div, point,
    px, relative,
};

use crate::{
    app::window_has_rounded_corners,
    player::{PlaybackCacheConfig, PlaybackLanguagePreferences, TrackLanguage},
    theme::{self, ColorTheme},
};

use super::{
    editor::Editor,
    scrollbar::Scrollbar,
    settings_controls::{
        DropdownState, disk_cache_capacity_control, disk_cache_capacity_input, selector_row,
        settings_category_button, settings_sidebar, toggle_switch, track_language_selector,
    },
    settings_dialog::SettingsChanged,
};
use memory_budget::MemoryBudget;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum SettingsCategory {
    #[default]
    Appearance,
    Playback,
    Memory,
    Disk,
}

impl SettingsCategory {
    const ALL: [Self; 4] = [Self::Appearance, Self::Playback, Self::Memory, Self::Disk];

    fn title(self) -> &'static str {
        match self {
            Self::Appearance => "外观",
            Self::Playback => "播放",
            Self::Memory => "内存缓存",
            Self::Disk => "磁盘缓存",
        }
    }
}

pub(crate) struct UserSettingsDialogState {
    category: SettingsCategory,
    config: PlaybackCacheConfig,
    color_theme: ColorTheme,
    track_languages: PlaybackLanguagePreferences,
    dropdown: Entity<DropdownState>,
    disk_cache_gib: Entity<Editor>,
    scroll_handle: ScrollHandle,
}

impl EventEmitter<SettingsChanged> for UserSettingsDialogState {}

impl UserSettingsDialogState {
    pub(crate) fn new(config: &PlaybackCacheConfig, cx: &mut Context<Self>) -> Self {
        let config = config.clone().normalized();
        Self {
            category: SettingsCategory::default(),
            disk_cache_gib: disk_cache_capacity_input(
                config.disk_cache_max_bytes,
                |this, bytes, cx| {
                    if this.config.disk_cache_max_bytes != bytes {
                        this.config.disk_cache_max_bytes = bytes;
                        this.changed(cx);
                    }
                },
                cx,
            ),
            config,
            color_theme: theme::get(cx).selection,
            track_languages: PlaybackLanguagePreferences::get(cx),
            dropdown: cx.new(DropdownState::new),
            scroll_handle: ScrollHandle::new(),
        }
    }

    pub(crate) fn playback_config(&self) -> PlaybackCacheConfig {
        self.config.clone()
    }

    pub(crate) fn color_theme(&self) -> ColorTheme {
        self.color_theme
    }

    pub(crate) fn track_languages(&self) -> PlaybackLanguagePreferences {
        self.track_languages
    }

    fn changed(&self, cx: &mut Context<Self>) {
        cx.emit(SettingsChanged);
        cx.notify();
    }

    fn select_theme(&mut self, selection: ColorTheme, cx: &mut Context<Self>) {
        if self.color_theme != selection {
            theme::set(selection, cx);
            self.color_theme = theme::get(cx).selection;
            self.changed(cx);
        }
    }

    fn select_language(&mut self, language: TrackLanguage, audio: bool, cx: &mut Context<Self>) {
        let current = if audio {
            &mut self.track_languages.audio
        } else {
            &mut self.track_languages.subtitle
        };
        if *current != language {
            *current = language;
            self.track_languages.apply(cx);
            self.changed(cx);
        }
    }

    fn select_memory_budget(&mut self, budget: Option<MemoryBudget>, cx: &mut Context<Self>) {
        if let Some(budget) = budget {
            self.set_config(budget.apply(&self.config), cx);
        }
    }

    fn toggle_disk_cache(&mut self, cx: &mut Context<Self>) {
        self.config.disk_cache = !self.config.disk_cache;
        self.changed(cx);
    }

    fn set_config(&mut self, config: PlaybackCacheConfig, cx: &mut Context<Self>) {
        if self.config != config {
            self.config = config;
            self.changed(cx);
        }
    }

    fn language_control(&self, audio: bool, cx: &Context<Self>) -> impl IntoElement {
        let dialog = cx.entity();
        track_language_selector(
            if audio {
                ("audio-language-dropdown", "音轨语言")
            } else {
                ("subtitle-language-dropdown", "字幕语言")
            },
            self.dropdown.clone(),
            if audio {
                self.track_languages.audio
            } else {
                self.track_languages.subtitle
            },
            move |language, cx| {
                dialog.update(cx, |dialog, cx| dialog.select_language(language, audio, cx))
            },
        )
    }

    fn render_preferences(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);
        let budget = MemoryBudget::current(&self.config);
        let theme_dialog = cx.entity();
        let budget_dialog = cx.entity();
        let disk_dialog = cx.entity();
        let content = div().flex().flex_col().min_w_0().pb_6().child(
            div()
                .mt_2()
                .mb_3()
                .text_base()
                .text_color(theme.foreground)
                .child(self.category.title()),
        );
        match self.category {
            SettingsCategory::Appearance => content.child(section("主题", cx).child(setting_row(
                "user-setting-theme",
                "颜色主题",
                "选择你喜欢的明暗与配色，立即生效。",
                selector_row(
                    ("color-theme-dropdown", "颜色主题"),
                    self.dropdown.clone(),
                    ColorTheme::ALL.map(|selection| (selection.id(), selection.name(), selection)),
                    self.color_theme,
                    move |selection, cx| {
                        theme_dialog.update(cx, |dialog, cx| dialog.select_theme(selection, cx))
                    },
                ),
                cx,
            ))),
            SettingsCategory::Playback => content.child(
                section("首选语言", cx)
                    .child(setting_row(
                        "user-setting-audio",
                        "音轨语言",
                        "播放时优先选择此语言；Default 或无匹配时使用默认音轨。",
                        self.language_control(true, cx),
                        cx,
                    ))
                    .child(setting_row(
                        "user-setting-subtitle",
                        "字幕语言",
                        "播放时优先选择此语言；Default 或无匹配时使用默认字幕，可在详情页手动选择。",
                        self.language_control(false, cx),
                        cx,
                    )),
            ),
            SettingsCategory::Memory => content.child(
                section("缓存容量", cx)
                    .child(setting_row(
                        "user-setting-memory-budget",
                        "内存预算",
                        if budget.is_some() { "设置缓存可使用的内存容量，自动分配预读和回看空间。" }
                        else { "当前使用自定义内存预算，选择容量后会调整缓存空间分配。" },
                        selector_row(
                            ("memory-budget-dropdown", "内存预算"),
                            self.dropdown.clone(),
                            MemoryBudget::ALL
                                .map(|budget| (budget.id(), budget.label(), Some(budget)))
                                .into_iter()
                                .chain(budget.is_none().then_some((
                                    "memory-budget-custom",
                                    "自定义",
                                    None,
                                ))),
                            budget,
                            move |budget, cx| {
                                budget_dialog
                                    .update(cx, |dialog, cx| dialog.select_memory_budget(budget, cx))
                            },
                        ),
                        cx,
                    )),
            ),
            SettingsCategory::Disk => content.child(section("存储与清理", cx)
                    .child(setting_row(
                        "user-setting-disk",
                        "启用磁盘缓存",
                        "将较远的数据存入磁盘，扩大预读和回看范围。",
                        toggle_switch("启用磁盘缓存", self.config.disk_cache,
                            move |cx| disk_dialog.update(cx, |dialog, cx| dialog.toggle_disk_cache(cx)), cx),
                        cx,
                    ))
                    .child(setting_row("user-setting-disk-limit", "磁盘缓存上限", "限制磁盘缓存使用的空间。",
                        disk_cache_capacity_control(self.disk_cache_gib.clone(), self.config.disk_cache_max_bytes), cx))),
        }
    }

    fn render_sidebar(&self, rounded_window: bool, cx: &Context<Self>) -> impl IntoElement {
        settings_sidebar(rounded_window, cx).child(div().flex().flex_col().gap_1().children(
            SettingsCategory::ALL.map(|category| {
                let dialog = cx.entity();
                settings_category_button(category.title(), self.category == category, cx).on_click(
                    move |_, window, cx| {
                        dialog.update(cx, |dialog, cx| {
                            window.blur(cx);
                            dialog.category = category;
                            dialog.dropdown.update(cx, |state, cx| state.close(cx));
                            dialog.scroll_handle.set_offset(point(px(0.0), px(0.0)));
                            cx.notify();
                        })
                    },
                )
            }),
        ))
    }
}

impl Render for UserSettingsDialogState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dropdown = self.dropdown.clone();
        div()
            .id("user-settings-panel")
            .debug_selector(|| "user-settings-panel".into())
            .flex()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .child(self.render_sidebar(window_has_rounded_corners(window), cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(
                        div()
                            .id("user-settings-scroll")
                            .size_full()
                            .px_8()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .on_scroll_wheel(move |_, _, cx| {
                                dropdown.update(cx, |state, cx| state.close(cx))
                            })
                            .child(self.render_preferences(cx)),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .right_0()
                            .bottom_0()
                            .left_0()
                            .child(Scrollbar::vertical(&self.scroll_handle).right_inset(px(4.0))),
                    ),
            )
    }
}

fn section(title: &'static str, cx: &App) -> gpui::Div {
    let theme = theme::get(cx);
    div().flex().flex_col().child(
        div()
            .pb_2()
            .border_b_1()
            .border_color(theme.title_bar_border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(title),
    )
}

fn setting_row(
    id: &'static str,
    title: &'static str,
    description: &'static str,
    control: impl IntoElement,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);
    div()
        .id(id)
        .debug_selector(move || id.into())
        .flex()
        .items_center()
        .justify_between()
        .min_w_0()
        .gap_5()
        .py_3()
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(div().text_sm().text_color(theme.foreground).child(title))
                .child(
                    div()
                        .text_xs()
                        .line_height(relative(1.5))
                        .text_color(theme.muted_foreground)
                        .child(description),
                ),
        )
        .child(div().flex_shrink_0().child(control))
}

#[cfg(test)]
#[path = "user_settings_dialog_tests.rs"]
mod interaction_tests;
