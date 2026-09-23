use gpui::{FontWeight, ScrollStrategy, UniformListScrollHandle, img, uniform_list};

use crate::{
    emby::{EmbyImageRequest, EmbyImageType, ImageQuality},
    images::{cache as image_cache, loader::ImageLoader},
    ui::{scrollbar::Scrollbar, tooltip::text_tooltip},
};

use super::*;

mod metadata;
use metadata::episode_metadata_label;

const EPISODE_LIST_WIDTH_PX: f32 = 420.0;
const EPISODE_ROW_HEIGHT_PX: f32 = 120.0;
// Match the detail page's cached episode covers.
const EPISODE_IMAGE_MAX_WIDTH: u32 = 640;

pub(super) struct PlaybackEpisodeListState {
    pub(super) open: bool,
    scroll: UniformListScrollHandle,
    images: ImageLoader,
}

impl Default for PlaybackEpisodeListState {
    fn default() -> Self {
        Self {
            open: false,
            scroll: UniformListScrollHandle::new(),
            images: ImageLoader::with_limits(4, Duration::from_secs(30), 3),
        }
    }
}

impl PlaybackPage {
    fn has_episode_list(&self) -> bool {
        self.queue.current().is_some_and(|item| {
            item.series_id
                .as_deref()
                .is_some_and(|id| !id.trim().is_empty())
        })
    }

    pub(super) fn close_episode_list(&mut self, cx: &mut Context<Self>) -> bool {
        if !std::mem::take(&mut self.episode_list.open) {
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
        if self.close_episode_list(cx) || !self.has_episode_list() || self.queue_switch.loading {
            return;
        }
        self.close_track_select(cx);
        self.episode_list.open = true;
        self.fullscreen.controls_visible = true;
        self.fullscreen.cursor_visible = true;
        self.episode_list
            .scroll
            .scroll_to_item(self.queue.current_index, ScrollStrategy::Center);
        let first_visible = self.queue.current_index.saturating_sub(3);
        for item in self.queue.items[first_visible..]
            .iter()
            .chain(&self.queue.items[..first_visible])
        {
            if let Some(tag) = &item.primary_image_tag {
                self.episode_list.images.ensure_image(
                    &self.emby.server,
                    EmbyImageRequest::primary(item.item_id.clone(), Some(tag.clone()))
                        .with_max_width(EPISODE_IMAGE_MAX_WIDTH),
                );
            }
        }
        self.load_episode_images(cx);
        cx.notify();
    }

    fn load_episode_images(&mut self, cx: &mut Context<Self>) {
        if !self.episode_list.open {
            return;
        }
        for job in self.episode_list.images.start_queued_jobs() {
            let client = self.emby.client.clone();
            let server = self.emby.server.clone();
            let key = job.key.clone();
            let task = cx.background_spawn(async move {
                let image = client.item_image(&server, &job.request)?;
                let path = image_cache::write_cached_image(
                    &job.key,
                    &image.bytes,
                    image.content_type.as_deref(),
                )?;
                let _ = image_cache::prune_cache(image_cache::DEFAULT_MAX_CACHE_BYTES);
                Ok(path)
            });
            cx.spawn(async move |page, cx| {
                let result = task.await;
                page.update(cx, |page, cx| {
                    page.episode_list.images.finish_job(key, result);
                    page.load_episode_images(cx);
                    if page.episode_list.open {
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    pub(super) fn render_episode_list_button(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::media_overlay(cx);
        let enabled = self.has_episode_list() && !self.queue_switch.loading;
        Self::playback_control_button(
            "playback-episodes-button",
            "icons/columns.svg",
            px(30.0),
            px(16.0),
            enabled,
            cx,
        )
        .aria_label("剧集列表")
        .tooltip(|_, cx| text_tooltip("剧集列表", cx))
        .when(self.episode_list.open, |this| {
            this.bg(theme.element_selected)
        })
        .when(enabled, |this| {
            this.on_mouse_down(MouseButton::Left, cx.listener(Self::toggle_episode_list))
        })
    }

    pub(super) fn render_episode_list_backdrop(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .id("playback-episodes-backdrop")
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
        let count = self.queue.items.len();
        let scroll = &self.episode_list.scroll;
        div()
            .id("playback-episodes-panel")
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
                        Self::playback_control_button(
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

    fn episode_file_size(&self, index: usize) -> Option<u64> {
        let item = self.queue.items.get(index)?;
        if index == self.queue.current_index {
            return self.content_length.filter(|size| *size > 0).or_else(|| {
                [
                    &self.emby.media_source_id,
                    &self.track_preference_key.media_source_id,
                ]
                .into_iter()
                .find_map(|source_id| {
                    item.media_sources
                        .iter()
                        .find(|source| source.id.as_ref() == Some(source_id))
                        .and_then(|source| source.size)
                        .filter(|size| *size > 0)
                })
            });
        }
        request::preferred_playback_media_source(&item.media_sources)
            .and_then(|source| source.size)
            .filter(|size| *size > 0)
    }

    fn render_episode_card(&self, index: usize, cx: &Context<Self>) -> gpui::Div {
        let theme = theme::media_overlay(cx);
        let item = &self.queue.items[index];
        let selected = index == self.queue.current_index;
        let label = item.episode_label.clone();
        let metadata = episode_metadata_label(item, self.episode_file_size(index));
        let overview = item
            .overview
            .as_deref()
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|text| !text.is_empty());
        let image_path = self.episode_list.images.path_for_source(
            &self.emby.server,
            &item.item_id,
            EmbyImageType::Primary,
            item.primary_image_tag.as_deref(),
            Some(EPISODE_IMAGE_MAX_WIDTH),
            ImageQuality::DEFAULT,
        );

        div().h(px(EPISODE_ROW_HEIGHT_PX)).pb_2().child(
            div()
                .id((
                    gpui::ElementId::from("playback-episode"),
                    item.item_id.clone(),
                ))
                .debug_selector(move || format!("playback-episode-{index}"))
                .size_full()
                .flex()
                .items_center()
                .gap_2()
                .p_2()
                .rounded(px(8.0))
                .border_1()
                .border_color(if selected {
                    theme.input_border_focused
                } else {
                    theme.input_border.opacity(0.32)
                })
                .when(selected, |this| this.bg(theme.element_selected))
                .cursor_pointer()
                .hover(move |style| {
                    style.bg(if selected {
                        theme.element_selected_hover
                    } else {
                        theme.secondary_hover
                    })
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |page, _, window, cx| {
                        cx.stop_propagation();
                        if selected {
                            page.close_episode_list(cx);
                        } else {
                            page.switch_to_episode(index, window, cx);
                        }
                    }),
                )
                .child(
                    div()
                        .debug_selector(move || format!("playback-episode-image-{index}"))
                        .flex_none()
                        .relative()
                        .w(px(144.0))
                        .h(px(81.0))
                        .rounded(px(6.0))
                        .overflow_hidden()
                        .bg(theme.input_background)
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(image_path.is_none(), |this| {
                            this.child(
                                svg()
                                    .path("icons/clapperboard.svg")
                                    .size(px(24.0))
                                    .text_color(theme.muted_foreground),
                            )
                        })
                        .when_some(image_path, |this, path| {
                            this.child(
                                img(path)
                                    .size_full()
                                    .rounded(px(6.0))
                                    .object_fit(gpui::ObjectFit::Cover),
                            )
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .id("episode-label")
                                .debug_selector(move || format!("playback-episode-label-{index}"))
                                .truncate()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.foreground)
                                .child(label.clone())
                                .tooltip(move |_, cx| text_tooltip(label.clone(), cx)),
                        )
                        .when_some(metadata, |this, metadata| {
                            this.child(
                                div()
                                    .debug_selector(move || {
                                        format!("playback-episode-metadata-{index}")
                                    })
                                    .truncate()
                                    .text_xs()
                                    .line_height(px(16.0))
                                    .text_color(theme.muted_foreground)
                                    .child(metadata),
                            )
                        })
                        .when_some(overview, |this, overview| {
                            this.child(
                                div()
                                    .id("episode-overview")
                                    .debug_selector(move || {
                                        format!("playback-episode-overview-{index}")
                                    })
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .text_ellipsis()
                                    .line_clamp(2)
                                    .child(overview.clone())
                                    .tooltip(move |_, cx| text_tooltip(overview.clone(), cx)),
                            )
                        }),
                ),
        )
    }
}

#[cfg(test)]
pub(in crate::player::page) mod tests;
