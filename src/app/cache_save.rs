use std::time::Duration;

use gpui::{AppContext as _, Context, Timer};

use crate::storage;

use super::TinyApp;

const CACHE_SAVE_DEBOUNCE: Duration = Duration::from_millis(350);

impl TinyApp {
    pub(super) fn schedule_cache_save(
        &mut self,
        error_prefix: &'static str,
        cx: &mut Context<Self>,
    ) {
        self.cache_save_generation = self.cache_save_generation.wrapping_add(1);
        self.pending_cache_save_error_prefix = Some(error_prefix);
        let generation = self.cache_save_generation;

        cx.spawn(async move |app, cx| {
            Timer::after(CACHE_SAVE_DEBOUNCE).await;
            app.update(cx, |app, cx| {
                app.flush_scheduled_cache_save(generation, cx);
            })
            .ok();
        })
        .detach();
    }

    fn flush_scheduled_cache_save(&mut self, generation: u64, cx: &mut Context<Self>) {
        if self.cache_save_generation != generation {
            return;
        }

        let Some(error_prefix) = self.pending_cache_save_error_prefix.take() else {
            return;
        };
        let cache = self.cache.clone();
        let task = cx.background_spawn(async move { storage::save(&cache) });

        cx.spawn(async move |app, cx| {
            let result = task.await;
            app.update(cx, |app, cx| {
                if let Err(error) = result {
                    app.cache_error = Some(format!("{error_prefix}：{error}").into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}
