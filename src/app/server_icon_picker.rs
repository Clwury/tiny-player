use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, FocusHandle, InteractiveElement,
    IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, UniformListScrollHandle, Window, div, prelude::FluentBuilder, px, svg,
    uniform_list,
};
use uuid::Uuid;

use crate::{
    server::{CachedServer, icon::all_icons},
    theme,
    ui::{
        editor::{Editor, EditorEvent, Escape},
        scrollbar::Scrollbar,
        server_icon::{cache_server_icon, clear_icon_previews, icon_preview, reload_server_icon},
    },
};

use super::{Page, TinyApp, window::window_has_rounded_corners};

pub(super) struct ServerIconPicker {
    server_id: String,
    session: Uuid,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    scroll: UniformListScrollHandle,
    search: Entity<Editor>,
    _search_subscription: Subscription,
    selected_url: Option<String>,
    pending_url: Option<String>,
    error: Option<SharedString>,
    task: Task<()>,
}

impl ServerIconPicker {
    pub(super) fn clear_previews(&self, cx: &mut App) {
        clear_icon_previews(self.session, cx);
    }
}

impl TinyApp {
    pub(super) fn open_server_icon_picker(
        &mut self,
        server: &CachedServer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_server_menu(window, cx);
        if !matches!(self.page, Page::Servers)
            || self.selecting_server_id.is_some()
            || self.add_server_dialog.is_some()
            || self.server_icon_picker.is_some()
        {
            return;
        }
        let Some(server) = self
            .cache
            .servers
            .iter()
            .find(|saved| saved.id == server.id)
        else {
            return;
        };
        let focus = cx.focus_handle();
        let previous_focus = window.focused(cx);
        focus.focus(window, cx);
        let session = Uuid::new_v4();
        let search = cx.new(|cx| Editor::new("搜索图标名称…", cx).borderless().clearable());
        let search_subscription = cx.subscribe(&search, move |app, _, event, cx| {
            if matches!(event, EditorEvent::Changed)
                && let Some(picker) = app
                    .server_icon_picker
                    .as_mut()
                    .filter(|picker| picker.session == session)
            {
                picker.scroll = UniformListScrollHandle::new();
                cx.notify();
            }
        });
        self.server_icon_picker = Some(ServerIconPicker {
            server_id: server.id.clone(),
            session,
            focus,
            previous_focus,
            scroll: UniformListScrollHandle::new(),
            search,
            _search_subscription: search_subscription,
            selected_url: server.icon_url.clone(),
            pending_url: None,
            error: None,
            task: Task::ready(()),
        });
        cx.notify();
    }

    pub(super) fn dismiss_server_icon_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(picker) = self.server_icon_picker.take() {
            picker.clear_previews(cx);
            if let Some(focus) = picker.previous_focus {
                focus.focus(window, cx);
            } else {
                window.blur(cx);
            }
            cx.notify();
        }
    }

    fn select_server_icon(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = self.server_icon_picker.as_mut() else {
            return;
        };
        if picker.pending_url.is_some() {
            return;
        }
        let Some(icon) = all_icons().get(index) else {
            return;
        };
        let url = icon.url.clone();
        let session = picker.session;
        picker.pending_url = Some(url.clone());
        picker.error = None;
        let download_url = url.clone();
        let download =
            cx.background_spawn(async move { cache_server_icon(&download_url).map(|_| ()) });
        picker.task = cx.spawn_in(window, async move |app, cx: &mut AsyncWindowContext| {
            let result = download.await;
            app.update_in(cx, |app, window, cx| {
                app.finish_select_server_icon(session, url, result, window, cx);
            })
            .ok();
        });
        cx.notify();
    }

    fn finish_select_server_icon(
        &mut self,
        session: Uuid,
        url: String,
        result: anyhow::Result<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(picker) = self
            .server_icon_picker
            .as_ref()
            .filter(|picker| picker.session == session)
        else {
            return;
        };
        let server_id = picker.server_id.clone();
        let result = result.and_then(|()| {
            // Merge into the latest server so concurrent count updates are preserved.
            let mut server = self
                .cache
                .servers
                .iter()
                .find(|server| server.id == server_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("服务器不存在"))?;
            server.icon_url = Some(url.clone());
            server.icon_is_custom = true;
            self.save_server(server, true)?;
            Ok(())
        });
        match result {
            Ok(()) => {
                self.servers = self.cache.servers.clone();
                reload_server_icon(&url, cx);
                self.dismiss_server_icon_picker(window, cx);
            }
            Err(error) => {
                let picker = self.server_icon_picker.as_mut().unwrap();
                picker.pending_url = None;
                picker.error = Some(format!("选择图标失败：{error}").into());
                cx.notify();
            }
        }
    }

    pub(super) fn render_server_icon_picker(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let picker = self.server_icon_picker.as_ref()?;
        let theme = theme::get(cx);
        let width = (window.viewport_size().width - px(48.0)).min(px(760.0));
        let height = (window.viewport_size().height - px(48.0)).min(px(600.0));
        let columns = ((f32::from(width) - 56.0) / 96.0).floor().max(1.0) as usize;
        let cell_width = (f32::from(width) - 56.0) / columns as f32;
        let session = picker.session;
        let scroll = picker.scroll.0.borrow().base_handle.clone();
        let query = picker.search.read(cx).value().trim().to_lowercase();
        let matching_icons: Vec<_> = all_icons()
            .iter()
            .enumerate()
            .filter_map(|(index, icon)| icon.name.to_lowercase().contains(&query).then_some(index))
            .collect();
        let match_count = matching_icons.len();

        let grid = uniform_list(
            "server-icon-grid",
            match_count.div_ceil(columns),
            cx.processor(move |app, rows: std::ops::Range<usize>, _, cx| {
                if !app
                    .server_icon_picker
                    .as_ref()
                    .is_some_and(|picker| picker.session == session)
                {
                    return Vec::new();
                }
                rows.map(|row| {
                    div().flex().h(px(96.0)).children(
                        (row * columns..((row + 1) * columns).min(matching_icons.len())).map(
                            |position| {
                                app.render_icon_choice(matching_icons[position], cell_width, cx)
                            },
                        ),
                    )
                })
                .collect()
            }),
        )
        .size_full()
        .track_scroll(&picker.scroll);
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_lg()
                    .text_color(theme.foreground)
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("选择图标"),
            )
            .child(
                div()
                    .id("close-server-icon-picker")
                    .debug_selector(|| "close-server-icon-picker".into())
                    .aria_label("关闭")
                    .cursor_pointer()
                    .flex()
                    .flex_none()
                    .size(px(28.0))
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .hover(move |style| style.bg(theme.secondary_hover))
                    .child(
                        svg()
                            .path("icons/window-close.svg")
                            .size(px(18.0))
                            .text_color(theme.foreground),
                    )
                    .on_click(cx.listener(|app, _, window, cx| {
                        cx.stop_propagation();
                        app.dismiss_server_icon_picker(window, cx);
                    })),
            );
        let panel =
            div()
                .id("server-icon-picker")
                .debug_selector(|| "server-icon-picker".into())
                .flex()
                .flex_col()
                .w(width)
                .h(height)
                .p_5()
                .gap_3()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.input_border)
                .bg(theme.dialog_background)
                .shadow_lg()
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(header)
                .child(
                    div()
                        .id("server-icon-search")
                        .debug_selector(|| "server-icon-search".into())
                        .flex()
                        .flex_none()
                        .items_center()
                        .pl_2()
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(theme.input_border)
                        .bg(theme.input_background)
                        .child(
                            svg()
                                .path("icons/search.svg")
                                .size(px(16.0))
                                .flex_none()
                                .text_color(theme.muted_foreground),
                        )
                        .child(div().flex_1().min_w_0().child(picker.search.clone())),
                )
                .child(div().text_sm().text_color(theme.muted_foreground).child(
                    if query.is_empty() {
                        format!("共 {match_count} 个图标 · 点击图标即可应用")
                    } else {
                        format!("找到 {match_count} 个图标 · 点击图标即可应用")
                    },
                ))
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .when(match_count > 0, |this| {
                            this.child(grid).child(Scrollbar::vertical(&scroll))
                        })
                        .when(match_count == 0, |this| {
                            this.child(
                                div()
                                    .id("server-icon-search-empty")
                                    .debug_selector(|| "server-icon-search-empty".into())
                                    .flex()
                                    .size_full()
                                    .items_center()
                                    .justify_center()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child("没有找到匹配的图标"),
                            )
                        }),
                )
                .when(picker.pending_url.is_some(), |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("正在应用图标…"),
                    )
                })
                .when_some(picker.error.clone(), |this, error| {
                    this.child(div().text_sm().text_color(theme.error).child(error))
                });

        Some(
            div()
                .id("server-icon-overlay")
                .debug_selector(|| "server-icon-overlay".into())
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .left_0()
                .occlude()
                .track_focus(&picker.focus)
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.overlay)
                .when(window_has_rounded_corners(window), |this| {
                    this.rounded(theme.radius_lg).overflow_hidden()
                })
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|app, _, window, cx| {
                    cx.stop_propagation();
                    app.dismiss_server_icon_picker(window, cx);
                }))
                .on_key_down(cx.listener(|app, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        app.dismiss_server_icon_picker(window, cx);
                    }
                }))
                .capture_action(cx.listener(|app, _: &Escape, window, cx| {
                    cx.stop_propagation();
                    app.dismiss_server_icon_picker(window, cx);
                }))
                .child(panel)
                .into_any_element(),
        )
    }

    fn render_icon_choice(&self, index: usize, width: f32, cx: &Context<Self>) -> impl IntoElement {
        let picker = self.server_icon_picker.as_ref().unwrap();
        let theme = theme::get(cx);
        let icon = &all_icons()[index];
        let selected = picker.selected_url.as_deref() == Some(&icon.url);
        let pending = picker.pending_url.as_deref() == Some(&icon.url);
        let choice = div()
            .id(("server-icon-choice", index))
            .debug_selector(move || format!("server-icon-choice-{index}"))
            .aria_label(icon.name.clone())
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .size_full()
            .gap_1()
            .px_1()
            .rounded(px(8.0))
            .when(selected, |this| this.bg(theme.element_selected))
            .when(pending, |this| this.opacity(0.5))
            .when(picker.pending_url.is_none(), |this| {
                this.cursor_pointer()
                    .hover(move |style| {
                        style.bg(if selected {
                            theme.element_selected_hover
                        } else {
                            theme.secondary_hover
                        })
                    })
                    .on_click(cx.listener(move |app, _, window, cx| {
                        cx.stop_propagation();
                        app.select_server_icon(index, window, cx);
                    }))
            })
            .child(icon_preview(picker.session, &icon.url, 48.0))
            .child(
                div()
                    .w_full()
                    .text_xs()
                    .text_center()
                    .text_ellipsis()
                    .text_color(theme.foreground)
                    .child(icon.name.clone()),
            );
        div().w(px(width)).h_full().p_1().child(choice)
    }
}

#[cfg(test)]
mod tests;
