use std::time::Duration;

use gpui::{AppContext as _, Context};

use crate::storage;

use super::TinyApp;

const CACHE_SAVE_DEBOUNCE: Duration = Duration::from_millis(350);

impl TinyApp {
    pub(super) fn schedule_cache_save(
        &mut self,
        error_prefix: &'static str,
        cx: &mut Context<Self>,
    ) {
        self.pending_cache_save_error_prefix = Some(error_prefix);
        self.last_cache_save_activity = Some(std::time::Instant::now());

        // Keep one debounce task alive for the whole burst of changes. Window
        // resize events can arrive once per frame; creating and cancelling a
        // timer for every pixel change still allocates work on the foreground
        // executor even though only the final size needs to be persisted.
        if self.cache_save_task_active {
            return;
        }

        self.cache_save_task_active = true;
        self.cache_save_task = cx.spawn(async move |app, cx| {
            cx.background_executor().timer(CACHE_SAVE_DEBOUNCE).await;
            loop {
                let remaining = app
                    .update(cx, |app, _| {
                        app.last_cache_save_activity
                            .map(|last| CACHE_SAVE_DEBOUNCE.saturating_sub(last.elapsed()))
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                if remaining.is_zero() {
                    app.update(cx, |app, cx| {
                        app.cache_save_task_active = false;
                        app.last_cache_save_activity = None;
                        app.flush_scheduled_cache_save(cx);
                    })
                    .ok();
                    break;
                }
                cx.background_executor().timer(remaining).await;
            }
        });
    }

    fn flush_scheduled_cache_save(&mut self, cx: &mut Context<Self>) {
        let Some(error_prefix) = self.pending_cache_save_error_prefix.take() else {
            return;
        };
        let cache = self.cache.clone();
        let task = cx.background_spawn(async move { storage::save(&cache) });

        cx.spawn(async move |app, cx| {
            let result = task.await;
            app.update(cx, |app, cx| {
                if let Err(error) = result {
                    app.push_app_error_notification(format!("{error_prefix}：{error}"), cx);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}
