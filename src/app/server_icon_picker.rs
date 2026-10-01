use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, FocusHandle, Subscription,
    UniformListScrollHandle, Window,
};
use uuid::Uuid;

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
    ui::editor::{Editor, EditorEvent},
};

use super::{Page, TinyApp, window_corner_radii};

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
        use crate::server::view::icon_picker::{
            IconPickerIntent, IconPickerProps, IconPickerSelection, icon_picker,
        };
        let picker = self.server_icon_picker.as_ref()?;
        let vm = self.server_feature.icon_picker()?;
        let session = picker.session;
        let server_id = vm.server_id.to_owned();
        let target_id = server_id.clone();
        let listener = cx.listener(
            move |app: &mut Self, intent: &IconPickerIntent, window, cx| {
                if !app
                    .server_icon_picker
                    .as_ref()
                    .is_some_and(|picker| picker.session == session)
                    || !app
                        .server_feature
                        .icon_picker()
                        .is_some_and(|vm| vm.server_id == target_id)
                {
                    return;
                }
                match intent {
                    IconPickerIntent::Close => app.dismiss_server_icon_picker(window, cx),
                    IconPickerIntent::Select(index) => app.select_server_icon(*index, window, cx),
                }
            },
        );
        Some(icon_picker(
            IconPickerProps {
                session,
                focus: picker.focus.clone(),
                scroll: picker.scroll.clone(),
                search: picker.search.clone(),
                corners: window_corner_radii(window, cx),
                catalog: all_icons(),
                view: vm,
            },
            std::rc::Rc::new(move |intent, window, cx| listener(&intent, window, cx)),
            move |app: &Self| {
                if !app
                    .server_icon_picker
                    .as_ref()
                    .is_some_and(|picker| picker.session == session)
                {
                    return None;
                }
                let vm = app.server_feature.icon_picker()?;
                (vm.server_id == server_id).then(|| IconPickerSelection {
                    selected_url: vm.selected_url.map(str::to_owned),
                    pending_url: vm.pending_url.map(str::to_owned),
                })
            },
            window,
            cx,
        ))
    }
}

#[cfg(test)]
mod tests;
