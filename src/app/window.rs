use gpui::{Context, SharedString, Window};

use crate::storage;

use super::{Page, TinyApp};

impl TinyApp {
    pub(super) fn observe_window_bounds_once(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.window_bounds_observed {
            return;
        }

        cx.observe_window_bounds(window, |app, window, cx| {
            app.save_window_size(window, cx);
        })
        .detach();
        self.window_bounds_observed = true;
    }

    pub(super) fn title(&self, cx: &Context<Self>) -> SharedString {
        match &self.page {
            Page::Servers => "Tiny".into(),
            Page::Home(page) => page.read(cx).title().into(),
        }
    }

    fn save_window_size(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.window_persistence_enabled || window.is_maximized() || window.is_fullscreen() {
            return;
        }

        let size = window.window_bounds().get_bounds().size;
        let width = f32::from(size.width).round();
        let height = f32::from(size.height).round();
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return;
        }

        let width = width as u32;
        let height = height as u32;
        if !self.cache.set_window_size(width, height) {
            return;
        }

        if let Err(error) = storage::save(&self.cache) {
            self.cache_error = Some(format!("保存窗口大小失败：{error}").into());
            cx.notify();
        }
    }
}
