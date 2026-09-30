use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, FocusHandle, InteractiveElement,
    IntoElement, MouseButton, ParentElement, StatefulInteractiveElement, Styled, Subscription,
    UniformListScrollHandle, Window, div, prelude::FluentBuilder, px, svg, uniform_list,
};
use uuid::Uuid;

use crate::ui::radius;
use crate::{
    images::{
        FileImageRepository,
        server_icon_assets::{clear_icon_previews, reload_server_icon},
    },
    server::{
        CachedServer,
        feature::{
            IconDownloadResult, IconRequest, ServerCommand, ServerIntent, effect::download_icon,
        },
        icon::all_icons,
    },
    theme,
    ui::{
        editor::{Editor, EditorEvent, Escape},
        scrollbar::Scrollbar,
        server_icon::icon_preview,
    },
};

use super::{Page, TinyApp, WindowCornersExt, window_corner_radii};

pub(super) struct ServerIconPicker {
    session: Uuid,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    scroll: UniformListScrollHandle,
    search: Entity<Editor>,
    _search_subscription: Subscription,
}

impl ServerIconPicker {
    pub(super) fn clear_previews(&self, cx: &mut App) {
        clear_icon_previews(
            self.session,
            all_icons().iter().map(|icon| icon.url.as_str()),
            cx,
        );
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
        if !matches!(self.shell.page(), Page::Servers)
            || self.server_feature.selecting_server_id().is_some()
            || self.add_server_dialog.is_some()
            || self.server_icon_picker.is_some()
        {
            return;
        }
        if !matches!(
            self.server_feature
                .dispatch(ServerIntent::OpenIconPicker(server.id.clone())),
            ServerCommand::PreviewChanged
        ) {
            return;
        }
        let focus = cx.focus_handle();
        let previous_focus = window.focused(cx);
        focus.focus(window, cx);
        let session = Uuid::new_v4();
        let search = cx.new(|cx| Editor::new("搜索图标名称…", cx).search());
        let search_subscription = cx.subscribe(&search, move |app, editor, event, cx| {
            if matches!(event, EditorEvent::Changed)
                && let Some(picker) = app
                    .server_icon_picker
                    .as_mut()
                    .filter(|picker| picker.session == session)
            {
                picker.scroll = UniformListScrollHandle::new();
                let query = editor.read(cx).value().to_string();
                app.server_feature
                    .dispatch(ServerIntent::SearchIcons(query));
                cx.notify();
            }
        });
        self.server_icon_picker = Some(ServerIconPicker {
            session,
            focus,
            previous_focus,
            scroll: UniformListScrollHandle::new(),
            search,
            _search_subscription: search_subscription,
        });
        cx.notify();
    }

    pub(super) fn dismiss_server_icon_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.server_effects.icon.cancel();
        self.server_feature.dispatch(ServerIntent::CloseIconPicker);
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
        if self.server_icon_picker.is_none() {
            return;
        }
        let ServerCommand::DownloadIcon(request) = self
            .server_feature
            .dispatch(ServerIntent::SelectIcon(index))
        else {
            return;
        };
        let download_request = request.clone();
        let download =
            cx.background_spawn(
                async move { download_icon(&FileImageRepository, &download_request) },
            );
        self.server_effects.icon.replace(cx.spawn_in(
            window,
            async move |app, cx: &mut AsyncWindowContext| {
                let result = download.await;
                app.update_in(cx, |app, window, cx| {
                    app.finish_select_server_icon(&request, result, window, cx);
                })
                .ok();
            },
        ));
        cx.notify();
    }

    fn finish_select_server_icon(
        &mut self,
        request: &IconRequest,
        result: anyhow::Result<()>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let server = match self.server_feature.finish_icon_download(request, result) {
            IconDownloadResult::Ignored => return,
            IconDownloadResult::Invalidated | IconDownloadResult::Failed => {
                self.server_effects.icon.cancel();
                cx.notify();
                return;
            }
            IconDownloadResult::Save(server) => server,
        };
        let result = self.save_server(*server, true).map(|_| ());
        self.server_feature.finish_icon_save(request, result);
        self.server_effects.icon.cancel();
        if self.server_feature.icon_picker().is_none() {
            reload_server_icon(&request.url, cx);
            self.dismiss_server_icon_picker(window, cx);
        } else {
            cx.notify();
        }
    }

    pub(super) fn render_server_icon_picker(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let picker = self.server_icon_picker.as_ref()?;
        let vm = self.server_feature.icon_picker()?;
        let theme = theme::get(cx);
        let width = (window.viewport_size().width - px(48.0)).min(px(760.0));
        let height = (window.viewport_size().height - px(48.0)).min(px(600.0));
        let columns = ((f32::from(width) - 56.0) / 96.0).floor().max(1.0) as usize;
        let cell_width = (f32::from(width) - 56.0) / columns as f32;
        let session = picker.session;
        let scroll = picker.scroll.0.borrow().base_handle.clone();
        let query = vm.query;
        let server_id = vm.server_id.to_owned();
        let matching_icons = vm.matching_icons.clone();
        let match_count = matching_icons.len();

        let grid = uniform_list(
            "server-icon-grid",
            match_count.div_ceil(columns),
            cx.processor(move |app, rows: std::ops::Range<usize>, _, cx| {
                if !app
                    .server_icon_picker
                    .as_ref()
                    .is_some_and(|picker| picker.session == session)
                    || !app
                        .server_feature
                        .icon_picker()
                        .is_some_and(|vm| vm.server_id == server_id)
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
                    .rounded(radius::CONTROL)
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
                .rounded(radius::SURFACE)
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
                        .child(picker.search.clone()),
                )
                .child(div().text_sm().text_color(theme.muted_foreground).child(
                    if query.is_empty() {
                        format!("共 {match_count} 个图标")
                    } else {
                        format!("找到 {match_count} 个图标")
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
                .when(vm.pending_url.is_some(), |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("正在应用图标…"),
                    )
                })
                .when_some(vm.error.map(str::to_owned), |this, error| {
                    this.child(div().text_sm().text_color(theme.error).child(error))
                });

        Some(
            div()
                .id("server-icon-overlay")
                .cursor_default()
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
                .rounded_window_corners(window_corner_radii(window, cx))
                .overflow_hidden()
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
        let vm = self.server_feature.icon_picker().unwrap();
        let theme = theme::get(cx);
        let icon = &all_icons()[index];
        let selected = vm.selected_url == Some(&icon.url);
        let pending = vm.pending_url == Some(&icon.url);
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
            .rounded(radius::CONTROL)
            .when(selected, |this| this.bg(theme.element_selected))
            .when(pending, |this| this.opacity(0.5))
            .cursor_default()
            .when(vm.pending_url.is_none(), |this| {
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
