use gpui::{App, Global, SharedString};

use super::{AppLanguage, translate};

impl Global for AppLanguage {}

impl AppLanguage {
    pub(crate) fn get(cx: &App) -> Self {
        cx.try_global::<Self>().copied().unwrap_or_default()
    }

    pub(crate) fn apply(self, cx: &mut App) {
        if cx.try_global::<Self>() != Some(&self) {
            cx.set_global(self);
            cx.refresh_windows();
        }
    }
}

pub(crate) fn tr(text: impl Into<SharedString>, cx: &App) -> SharedString {
    let text = text.into();
    match translate(AppLanguage::get(cx), &text) {
        std::borrow::Cow::Borrowed(_) => text,
        std::borrow::Cow::Owned(translated) => translated.into(),
    }
}
