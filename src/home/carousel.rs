use gpui::{ClickEvent, Context, Window};

use super::HomePage;

pub(super) const USER_VIEW_CARD_WIDTH_PX: f32 = 240.0;
pub(super) const USER_VIEW_CARD_PADDING_PX: f32 = 4.0;
pub(super) const USER_VIEW_CARD_GAP_PX: f32 = 16.0;
const USER_VIEW_SCROLL_CARD_COUNT: f32 = 3.0;
const USER_VIEW_CARD_OUTER_WIDTH_PX: f32 =
    USER_VIEW_CARD_WIDTH_PX + USER_VIEW_CARD_PADDING_PX * 2.0;
const USER_VIEW_CARD_STEP_PX: f32 = USER_VIEW_CARD_OUTER_WIDTH_PX + USER_VIEW_CARD_GAP_PX;
const USER_VIEW_SCROLL_STEP_PX: f32 = USER_VIEW_CARD_STEP_PX * USER_VIEW_SCROLL_CARD_COUNT;
pub(super) const USER_VIEW_CARD_IMAGE_HEIGHT_PX: f32 = 138.0;

pub(super) fn home_main_content_width(window: &Window) -> f32 {
    let window_width = f32::from(window.bounds().size.width);
    (window_width - 252.0 - 48.0).max(0.0)
}

pub(super) fn carousel_content_width(total: usize) -> f32 {
    if total == 0 {
        return 0.0;
    }

    total as f32 * USER_VIEW_CARD_OUTER_WIDTH_PX
        + total.saturating_sub(1) as f32 * USER_VIEW_CARD_GAP_PX
}

pub(super) fn max_carousel_scroll_offset(total: usize, viewport_width: f32) -> f32 {
    (carousel_content_width(total) - viewport_width).max(0.0)
}

impl HomePage {
    pub(super) fn set_user_views_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if self.user_views_hovered != hovered {
            self.user_views_hovered = hovered;
            cx.notify();
        }
    }

    pub(super) fn set_user_views_controls_hovered(
        &mut self,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        if self.user_views_controls_hovered != hovered {
            self.user_views_controls_hovered = hovered;
            cx.notify();
        }
    }

    pub(super) fn set_resume_items_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if self.resume_items_hovered != hovered {
            self.resume_items_hovered = hovered;
            cx.notify();
        }
    }

    pub(super) fn set_resume_items_controls_hovered(
        &mut self,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        if self.resume_items_controls_hovered != hovered {
            self.resume_items_controls_hovered = hovered;
            cx.notify();
        }
    }

    pub(super) fn scroll_user_views_left(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_user_views_scroll_offset(
            self.user_views_scroll_offset - USER_VIEW_SCROLL_STEP_PX,
            window,
            cx,
        );
    }

    pub(super) fn scroll_user_views_right(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_user_views_scroll_offset(
            self.user_views_scroll_offset + USER_VIEW_SCROLL_STEP_PX,
            window,
            cx,
        );
    }

    fn set_user_views_scroll_offset(
        &mut self,
        offset: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let viewport_width = home_main_content_width(window);
        let max_offset = self
            .user_views
            .as_ref()
            .map(|views| max_carousel_scroll_offset(views.items.len(), viewport_width))
            .unwrap_or(0.0);
        let offset = offset.clamp(0.0, max_offset);

        if self.user_views_scroll_offset == offset {
            return;
        }

        self.user_views_previous_scroll_offset = self.user_views_scroll_offset.min(max_offset);
        self.user_views_scroll_offset = offset;
        self.user_views_animation_id += 1;
        cx.notify();
    }

    pub(super) fn scroll_resume_items_left(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_resume_items_scroll_offset(
            self.resume_items_scroll_offset - USER_VIEW_SCROLL_STEP_PX,
            window,
            cx,
        );
    }

    pub(super) fn scroll_resume_items_right(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_resume_items_scroll_offset(
            self.resume_items_scroll_offset + USER_VIEW_SCROLL_STEP_PX,
            window,
            cx,
        );
    }

    fn set_resume_items_scroll_offset(
        &mut self,
        offset: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let viewport_width = home_main_content_width(window);
        let max_offset = self
            .resume_items
            .as_ref()
            .map(|items| max_carousel_scroll_offset(items.items.len(), viewport_width))
            .unwrap_or(0.0);
        let offset = offset.clamp(0.0, max_offset);

        if self.resume_items_scroll_offset == offset {
            return;
        }

        self.resume_items_previous_scroll_offset = self.resume_items_scroll_offset.min(max_offset);
        self.resume_items_scroll_offset = offset;
        self.resume_items_animation_id += 1;
        cx.notify();
    }
}
