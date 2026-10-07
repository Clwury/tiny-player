//! System activity requests. The playback page owns their lifetime.
use anyhow::Result;
use gpui::{ActivityGuard, App, Task, Window};

#[cfg(target_os = "linux")]
mod linux;

pub(super) const PLAYBACK_POWER_REASON: &str = "Tiny Player 正在播放视频";

pub(super) fn request(window: &Window, cx: &App) -> Task<Result<ActivityGuard>> {
    #[cfg(target_os = "linux")]
    {
        let _ = window;
        linux::request(cx.background_executor().clone())
    }
    #[cfg(target_os = "windows")]
    {
        let _ = cx;
        Task::ready(crate::app::prevent_playback_idle(
            window,
            PLAYBACK_POWER_REASON,
        ))
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = window;
        cx.prevent_idle_sleep(PLAYBACK_POWER_REASON)
    }
}
