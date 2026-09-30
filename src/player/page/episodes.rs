use gpui::{FontWeight, ScrollStrategy, UniformListScrollHandle, uniform_list};
use std::time::Instant;

use crate::{
    effects::EffectHandle,
    emby::{EmbyImageRequest, EmbyImageType, ImageQuality},
    images::{
        ImageRepository,
        cache::CachedImageKey,
        controller::{ImageController, ImageUpdate, ItemImageCommand, ItemImageRequest},
        item_images::EmbyImageRepository,
    },
    ui::{scrollbar::Scrollbar, tooltip::text_tooltip},
};

use super::*;

mod components;

const EPISODE_LIST_WIDTH_PX: f32 = 420.0;
const EPISODE_ROW_HEIGHT_PX: f32 = 120.0;
// Match the detail page's cached episode covers.
const EPISODE_IMAGE_MAX_WIDTH: u32 = 640;

pub(super) struct PlaybackEpisodeListState {
    pub(super) open: bool,
    scroll: UniformListScrollHandle,
    images: ImageController,
    image_effects: std::collections::HashMap<CachedImageKey, EffectHandle<gpui::Task<()>>>,
    image_repository:
        std::sync::Arc<dyn ImageRepository<ItemImageRequest, Image = std::path::PathBuf>>,
}

impl PlaybackEpisodeListState {
    pub(super) fn new(emby: &EmbyPlaybackContext) -> Self {
        Self {
            open: false,
            scroll: UniformListScrollHandle::new(),
            images: ImageController::with_limits(
                emby.server.workspace_identity(),
                4,
                Duration::from_secs(30),
                3,
            ),
            image_effects: Default::default(),
            image_repository: Arc::new(EmbyImageRepository {
                client: emby.client.clone(),
                server: emby.server.clone(),
            }),
        }
    }
}

impl PlaybackPage {
    pub(super) fn close_episode_list(&mut self, cx: &mut Context<Self>) -> bool {
        if !std::mem::take(&mut self.presentation.episode_list.open) {
            return false;
        }
        self.schedule_fullscreen_controls_hide(cx);
        cx.notify();
        true
    }

    fn dismiss_episode_list(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.close_episode_list(cx);
    }

    fn toggle_episode_list(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if self.close_episode_list(cx)
            || !self.session.queue.view_model().has_episode_list()
            || self.session.queue.view_model().loading
        {
            return;
        }
        self.close_track_select(cx);
        self.presentation.episode_list.open = true;
        self.presentation.fullscreen.controls_visible = true;
        self.presentation.fullscreen.cursor_visible = true;
        self.presentation.episode_list.scroll.scroll_to_item(
            self.session.queue.queue().current_index,
            ScrollStrategy::Center,
        );
        let first_visible = self.session.queue.queue().current_index.saturating_sub(3);
        for item in self.session.queue.queue().items[first_visible..]
            .iter()
            .chain(&self.session.queue.queue().items[..first_visible])
        {
            if let Some(tag) = &item.primary_image_tag {
                self.presentation.episode_list.images.ensure_image(
                    EmbyImageRequest::primary(item.item_id.clone(), Some(tag.clone()))
                        .with_max_width(EPISODE_IMAGE_MAX_WIDTH),
                    Instant::now(),
                );
            }
        }
        self.load_episode_images(cx);
        cx.notify();
    }

    fn load_episode_images(&mut self, cx: &mut Context<Self>) {
        if !self.presentation.episode_list.open {
            return;
        }
        for command in self.presentation.episode_list.images.start_queued_jobs() {
            let repository = self.presentation.episode_list.image_repository.clone();
            let task_image = command.image.clone();
            let key = command.image.key.clone();
            let task = cx.background_spawn(async move { repository.load(&task_image) });
            let handle = cx.spawn(async move |page, cx| {
                let result = task.await;
                page.update(cx, |page, cx| {
                    page.finish_episode_image(command, result, cx)
                })
                .ok();
            });
            self.presentation
                .episode_list
                .image_effects
                .entry(key)
                .or_default()
                .replace(handle);
        }
    }

    fn finish_episode_image(
        &mut self,
        command: ItemImageCommand,
        result: anyhow::Result<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let identity = self.emby.server.workspace_identity();
        let update = self.presentation.episode_list.images.finish_job(
            &command,
            result,
            &identity,
            Instant::now(),
        );
        if update == ImageUpdate::Ignored {
            return;
        }
        self.presentation
            .episode_list
            .image_effects
            .remove(&command.image.key);
        self.load_episode_images(cx);
        if self.presentation.episode_list.open && update == ImageUpdate::Ready {
            cx.notify();
        }
    }

    pub(super) fn render_episode_list_button(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::media_overlay(cx);
        let enabled = self.session.queue.view_model().has_episode_list()
            && !self.session.queue.view_model().loading;
        controls::playback_control_button(
            "playback-episodes-button",
            "icons/columns.svg",
            px(30.0),
            px(16.0),
            enabled,
            cx,
        )
        .aria_label("剧集列表")
        .tooltip(|_, cx| text_tooltip("剧集列表", cx))
        .when(self.presentation.episode_list.open, |this| {
            this.bg(theme.element_selected)
        })
        .when(enabled, |this| {
            this.on_mouse_down(MouseButton::Left, cx.listener(Self::toggle_episode_list))
        })
    }

    pub(super) fn render_episode_list_backdrop(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .id("playback-episodes-backdrop")
            .cursor_default()
            .absolute()
            // The video frame occupies the normal flow. Explicit insets keep
            // outside-click detection over the viewport instead of below it.
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .occlude()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::dismiss_episode_list))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::dismiss_episode_list))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::dismiss_episode_list))
            .on_mouse_down_out(cx.listener(|page, _, _, cx| {
                page.close_episode_list(cx);
            }))
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
    }

    pub(super) fn render_episode_list(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::media_overlay(cx);
        let count = self.session.queue.queue().items.len();
        let scroll = &self.presentation.episode_list.scroll;
        div()
            .id("playback-episodes-panel")
            .cursor_default()
            .debug_selector(|| "playback-episodes-panel".into())
            .absolute()
            .right_0()
            .top_0()
            .bottom_0()
            .w(px(EPISODE_LIST_WIDTH_PX))
            .max_w(relative(0.8))
            .flex()
            .flex_col()
            .rounded_br(window_corner_radii(window, cx).bottom_right)
            .border_l_1()
            .border_color(theme.input_border.opacity(0.72))
            .bg(theme.panel_background)
            .overflow_hidden()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .p_3()
                    .border_b_1()
                    .border_color(theme.input_border.opacity(0.42))
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(
                        controls::playback_control_button(
                            "playback-episodes-close",
                            "icons/window-close.svg",
                            px(24.0),
                            px(12.0),
                            true,
                            cx,
                        )
                        .aria_label("关闭剧集列表")
                        .on_mouse_down(MouseButton::Left, cx.listener(Self::dismiss_episode_list)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_right()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("剧集列表 · {count} 集")),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .m_2()
                    .child(
                        uniform_list(
                            "playback-episodes-list",
                            count,
                            cx.processor(|page, range: std::ops::Range<usize>, _, cx| {
                                range
                                    .map(|index| page.render_episode_card(index, cx))
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .debug_selector(|| "playback-episodes-list".into())
                        .size_full()
                        .pr(px(8.0))
                        .track_scroll(scroll),
                    )
                    .child(
                        Scrollbar::vertical(&scroll.0.borrow().base_handle)
                            .id("playback-episodes-scrollbar"),
                    ),
            )
    }

    fn render_episode_card(&self, index: usize, cx: &Context<Self>) -> gpui::Div {
        let Some(view) = self
            .session
            .episode_card_vm(index, &self.emby.media_source_id)
        else {
            return div().h(px(EPISODE_ROW_HEIGHT_PX));
        };
        let selected = view.selected;
        let image_path = self.presentation.episode_list.images.path_for_source(
            view.item_id,
            EmbyImageType::Primary,
            view.image_tag,
            Some(EPISODE_IMAGE_MAX_WIDTH),
            ImageQuality::DEFAULT,
        );
        components::episode_card(
            view,
            index,
            image_path,
            cx,
            cx.listener(move |page, _, window, cx| {
                cx.stop_propagation();
                if selected {
                    page.close_episode_list(cx);
                } else {
                    page.switch_to_episode(index, window, cx);
                }
            }),
        )
    }
}

#[cfg(test)]
pub(in crate::player::page) mod tests;
