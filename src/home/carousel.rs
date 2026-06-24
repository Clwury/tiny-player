use gpui::{ClickEvent, Context, Window};

use super::HomeContent;

pub(super) const USER_VIEW_CARD_WIDTH_PX: f32 = 240.0;
pub(super) const USER_VIEW_CARD_PADDING_PX: f32 = 4.0;
pub(super) const USER_VIEW_CARD_GAP_PX: f32 = 16.0;
const USER_VIEW_SCROLL_CARD_COUNT: f32 = 3.0;
const USER_VIEW_CARD_OUTER_WIDTH_PX: f32 =
    USER_VIEW_CARD_WIDTH_PX + USER_VIEW_CARD_PADDING_PX * 2.0;
const USER_VIEW_CARD_STEP_PX: f32 = USER_VIEW_CARD_OUTER_WIDTH_PX + USER_VIEW_CARD_GAP_PX;
const USER_VIEW_SCROLL_STEP_PX: f32 = USER_VIEW_CARD_STEP_PX * USER_VIEW_SCROLL_CARD_COUNT;
pub(super) const USER_VIEW_CARD_IMAGE_HEIGHT_PX: f32 = 138.0;
pub(super) const HOME_ITEM_CARD_WIDTH_PX: f32 = 160.0;
pub(super) const HOME_ITEM_CARD_IMAGE_HEIGHT_PX: f32 = 213.0;
pub(super) const HOME_ITEM_CARD_PADDING_PX: f32 = 4.0;
pub(super) const HOME_ITEM_CARD_GAP_PX: f32 = 16.0;
pub(super) const DETAIL_EPISODE_CARD_WIDTH_PX: f32 = 220.0;
pub(super) const DETAIL_EPISODE_CARD_IMAGE_HEIGHT_PX: f32 = 124.0;
pub(super) const DETAIL_EPISODE_CARD_PADDING_PX: f32 = 4.0;
pub(super) const DETAIL_EPISODE_CARD_GAP_PX: f32 = 16.0;
pub(super) const DETAIL_PERSON_CARD_WIDTH_PX: f32 = 140.0;
pub(super) const DETAIL_PERSON_CARD_IMAGE_WIDTH_PX: f32 = 140.0;
pub(super) const DETAIL_PERSON_CARD_IMAGE_HEIGHT_PX: f32 = 210.0;
pub(super) const DETAIL_PERSON_CARD_PADDING_PX: f32 = 4.0;
pub(super) const DETAIL_PERSON_CARD_GAP_PX: f32 = 16.0;
pub(super) const HOME_SIDEBAR_WIDTH_PX: f32 = 252.0;
pub(super) const HOME_MAIN_SCROLLBAR_WIDTH_PX: f32 = 12.0;
const HOME_MAIN_CONTENT_HORIZONTAL_PADDING_PX: f32 = 48.0;
const HOME_ITEM_SCROLL_CARD_COUNT: f32 = 4.0;
const HOME_ITEM_CARD_OUTER_WIDTH_PX: f32 =
    HOME_ITEM_CARD_WIDTH_PX + HOME_ITEM_CARD_PADDING_PX * 2.0;
const HOME_ITEM_CARD_STEP_PX: f32 = HOME_ITEM_CARD_OUTER_WIDTH_PX + HOME_ITEM_CARD_GAP_PX;
const HOME_ITEM_SCROLL_STEP_PX: f32 = HOME_ITEM_CARD_STEP_PX * HOME_ITEM_SCROLL_CARD_COUNT;
const DETAIL_EPISODE_CARD_OUTER_WIDTH_PX: f32 =
    DETAIL_EPISODE_CARD_WIDTH_PX + DETAIL_EPISODE_CARD_PADDING_PX * 2.0;
const DETAIL_EPISODE_CARD_STEP_PX: f32 =
    DETAIL_EPISODE_CARD_OUTER_WIDTH_PX + DETAIL_EPISODE_CARD_GAP_PX;
const DETAIL_EPISODE_SCROLL_CARD_COUNT: f32 = 3.0;
const DETAIL_EPISODE_SCROLL_STEP_PX: f32 =
    DETAIL_EPISODE_CARD_STEP_PX * DETAIL_EPISODE_SCROLL_CARD_COUNT;
const DETAIL_PERSON_CARD_OUTER_WIDTH_PX: f32 =
    DETAIL_PERSON_CARD_WIDTH_PX + DETAIL_PERSON_CARD_PADDING_PX * 2.0;
const DETAIL_PERSON_CARD_STEP_PX: f32 =
    DETAIL_PERSON_CARD_OUTER_WIDTH_PX + DETAIL_PERSON_CARD_GAP_PX;
const DETAIL_PERSON_SCROLL_CARD_COUNT: f32 = 5.0;
const DETAIL_PERSON_SCROLL_STEP_PX: f32 =
    DETAIL_PERSON_CARD_STEP_PX * DETAIL_PERSON_SCROLL_CARD_COUNT;

pub(super) fn home_main_content_width(window: &Window) -> f32 {
    let window_width = f32::from(window.bounds().size.width);
    (window_width
        - HOME_SIDEBAR_WIDTH_PX
        - HOME_MAIN_CONTENT_HORIZONTAL_PADDING_PX
        - HOME_MAIN_SCROLLBAR_WIDTH_PX)
        .max(0.0)
}

pub(super) fn carousel_content_width(total: usize) -> f32 {
    carousel_content_width_for(
        total,
        USER_VIEW_CARD_WIDTH_PX,
        USER_VIEW_CARD_PADDING_PX,
        USER_VIEW_CARD_GAP_PX,
    )
}

pub(super) fn carousel_content_width_for(
    total: usize,
    card_width: f32,
    card_padding: f32,
    gap: f32,
) -> f32 {
    if total == 0 {
        return 0.0;
    }

    total as f32 * (card_width + card_padding * 2.0) + total.saturating_sub(1) as f32 * gap
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct CarouselState {
    scroll_offset: f32,
    previous_scroll_offset: f32,
    animation_id: u64,
    hovered: bool,
    controls_hovered: bool,
}

impl CarouselState {
    pub(super) fn scroll_offset(self, max_offset: f32) -> f32 {
        self.scroll_offset.min(max_offset)
    }

    pub(super) fn previous_scroll_offset(self, max_offset: f32) -> f32 {
        self.previous_scroll_offset.min(max_offset)
    }

    pub(super) fn animation_id(self) -> u64 {
        self.animation_id
    }

    pub(super) fn controls_visible(self, has_controls: bool) -> bool {
        has_controls && (self.hovered || self.controls_hovered)
    }

    pub(super) fn set_hovered(&mut self, hovered: bool) -> bool {
        if self.hovered == hovered {
            return false;
        }

        self.hovered = hovered;
        true
    }

    pub(super) fn set_controls_hovered(&mut self, hovered: bool) -> bool {
        if self.controls_hovered == hovered {
            return false;
        }

        self.controls_hovered = hovered;
        true
    }

    pub(super) fn set_scroll_offset(&mut self, offset: f32, max_offset: f32) -> bool {
        let offset = offset.clamp(0.0, max_offset);
        if self.scroll_offset == offset {
            return false;
        }

        self.previous_scroll_offset = self.scroll_offset.min(max_offset);
        self.scroll_offset = offset;
        self.animation_id += 1;
        true
    }

    pub(super) fn sync_previous_offset(&mut self) {
        self.previous_scroll_offset = self.scroll_offset;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct CarouselVisibleRange {
    pub start: usize,
    pub end: usize,
    pub leading_width: f32,
    pub trailing_width: f32,
}

pub(super) fn carousel_visible_range_for(
    total: usize,
    scroll_offset: f32,
    viewport_width: f32,
    card_width: f32,
    card_padding: f32,
    gap: f32,
    overscan: (usize, usize),
) -> CarouselVisibleRange {
    if total == 0 {
        return CarouselVisibleRange {
            start: 0,
            end: 0,
            leading_width: 0.0,
            trailing_width: 0.0,
        };
    }

    let (overscan_before, overscan_after) = overscan;
    let card_outer_width = card_width + card_padding * 2.0;
    let card_step = card_outer_width + gap;
    let content_width = carousel_content_width_for(total, card_width, card_padding, gap);
    let max_offset = (content_width - viewport_width).max(0.0);
    let scroll_offset = scroll_offset.clamp(0.0, max_offset);
    let visible_start = (scroll_offset / card_step).floor().min((total - 1) as f32) as usize;
    let visible_end = ((scroll_offset + viewport_width) / card_step)
        .ceil()
        .max((visible_start + 1) as f32)
        .min(total as f32) as usize;
    carousel_visible_range_from_indices(
        total,
        visible_start,
        visible_end,
        card_outer_width,
        gap,
        overscan_before,
        overscan_after,
    )
}

pub(super) fn carousel_visible_range_between_for(
    total: usize,
    scroll_offsets: (f32, f32),
    viewport_width: f32,
    card_width: f32,
    card_padding: f32,
    gap: f32,
    overscan: (usize, usize),
) -> CarouselVisibleRange {
    let scroll_offset = scroll_offsets.0.min(scroll_offsets.1);
    let viewport_width = viewport_width + (scroll_offsets.0 - scroll_offsets.1).abs();
    carousel_visible_range_for(
        total,
        scroll_offset,
        viewport_width,
        card_width,
        card_padding,
        gap,
        overscan,
    )
}

fn carousel_visible_range_from_indices(
    total: usize,
    visible_start: usize,
    visible_end: usize,
    card_outer_width: f32,
    gap: f32,
    overscan_before: usize,
    overscan_after: usize,
) -> CarouselVisibleRange {
    let card_step = card_outer_width + gap;
    let start = visible_start.saturating_sub(overscan_before);
    let end = visible_end.saturating_add(overscan_after).min(total);
    let leading_width = if start == 0 {
        0.0
    } else {
        start as f32 * card_step - gap
    };
    let trailing_count = total - end;
    let trailing_width = if trailing_count == 0 {
        0.0
    } else {
        trailing_count as f32 * card_outer_width + trailing_count.saturating_sub(1) as f32 * gap
    };

    CarouselVisibleRange {
        start,
        end,
        leading_width,
        trailing_width,
    }
}

pub(super) fn max_carousel_scroll_offset(total: usize, viewport_width: f32) -> f32 {
    max_carousel_scroll_offset_for(
        total,
        viewport_width,
        USER_VIEW_CARD_WIDTH_PX,
        USER_VIEW_CARD_PADDING_PX,
        USER_VIEW_CARD_GAP_PX,
    )
}

pub(super) fn max_carousel_scroll_offset_for(
    total: usize,
    viewport_width: f32,
    card_width: f32,
    card_padding: f32,
    gap: f32,
) -> f32 {
    (carousel_content_width_for(total, card_width, card_padding, gap) - viewport_width).max(0.0)
}

impl HomeContent {
    pub(super) fn set_user_views_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if self.user_views_carousel.set_hovered(hovered) {
            cx.notify();
        }
    }

    pub(super) fn set_user_views_controls_hovered(
        &mut self,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        if self.user_views_carousel.set_controls_hovered(hovered) {
            cx.notify();
        }
    }

    pub(super) fn set_resume_items_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if self.resume_items_carousel.set_hovered(hovered) {
            cx.notify();
        }
    }

    pub(super) fn set_resume_items_controls_hovered(
        &mut self,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        if self.resume_items_carousel.set_controls_hovered(hovered) {
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
            self.user_views_carousel.scroll_offset(f32::INFINITY) - USER_VIEW_SCROLL_STEP_PX,
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
            self.user_views_carousel.scroll_offset(f32::INFINITY) + USER_VIEW_SCROLL_STEP_PX,
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
        if self
            .user_views_carousel
            .set_scroll_offset(offset, max_offset)
        {
            cx.notify();
        }
    }

    pub(super) fn scroll_resume_items_left(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_resume_items_scroll_offset(
            self.resume_items_carousel.scroll_offset(f32::INFINITY) - USER_VIEW_SCROLL_STEP_PX,
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
            self.resume_items_carousel.scroll_offset(f32::INFINITY) + USER_VIEW_SCROLL_STEP_PX,
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
        if self
            .resume_items_carousel
            .set_scroll_offset(offset, max_offset)
        {
            cx.notify();
        }
    }

    pub(super) fn set_user_view_items_hovered(
        &mut self,
        view_id: &str,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let row = self
            .user_view_items_rows
            .entry(view_id.to_string())
            .or_default();
        if row.carousel.set_hovered(hovered) {
            cx.notify();
        }
    }

    pub(super) fn set_user_view_items_controls_hovered(
        &mut self,
        view_id: &str,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let row = self
            .user_view_items_rows
            .entry(view_id.to_string())
            .or_default();
        if row.carousel.set_controls_hovered(hovered) {
            cx.notify();
        }
    }

    pub(super) fn scroll_user_view_items_left(
        &mut self,
        view_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self
            .user_view_items_rows
            .get(view_id)
            .map(|row| row.carousel.scroll_offset(f32::INFINITY) - HOME_ITEM_SCROLL_STEP_PX)
            .unwrap_or(0.0);
        self.set_user_view_items_scroll_offset(view_id, offset, window, cx);
    }

    pub(super) fn scroll_user_view_items_right(
        &mut self,
        view_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self
            .user_view_items_rows
            .get(view_id)
            .map(|row| row.carousel.scroll_offset(f32::INFINITY) + HOME_ITEM_SCROLL_STEP_PX)
            .unwrap_or(HOME_ITEM_SCROLL_STEP_PX);
        self.set_user_view_items_scroll_offset(view_id, offset, window, cx);
    }

    fn set_user_view_items_scroll_offset(
        &mut self,
        view_id: &str,
        offset: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let viewport_width = home_main_content_width(window);
        let max_offset = self
            .user_view_items_rows
            .get(view_id)
            .and_then(|row| row.items.as_ref())
            .map(|items| {
                max_carousel_scroll_offset_for(
                    items.items.len(),
                    viewport_width,
                    HOME_ITEM_CARD_WIDTH_PX,
                    HOME_ITEM_CARD_PADDING_PX,
                    HOME_ITEM_CARD_GAP_PX,
                )
            })
            .unwrap_or(0.0);
        let row = self
            .user_view_items_rows
            .entry(view_id.to_string())
            .or_default();

        if row.carousel.set_scroll_offset(offset, max_offset) {
            cx.notify();
        }
    }

    pub(super) fn set_series_episodes_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        let Some(detail) = &mut self.series_detail else {
            return;
        };
        if detail.episodes_carousel.set_hovered(hovered) {
            cx.notify();
        }
    }

    pub(super) fn set_series_episodes_controls_hovered(
        &mut self,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = &mut self.series_detail else {
            return;
        };
        if detail.episodes_carousel.set_controls_hovered(hovered) {
            cx.notify();
        }
    }

    pub(super) fn scroll_series_episodes_left(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self
            .series_detail
            .as_ref()
            .map(|detail| {
                detail.episodes_carousel.scroll_offset(f32::INFINITY)
                    - DETAIL_EPISODE_SCROLL_STEP_PX
            })
            .unwrap_or(0.0);
        self.set_series_episodes_scroll_offset(offset, window, cx);
    }

    pub(super) fn scroll_series_episodes_right(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self
            .series_detail
            .as_ref()
            .map(|detail| {
                detail.episodes_carousel.scroll_offset(f32::INFINITY)
                    + DETAIL_EPISODE_SCROLL_STEP_PX
            })
            .unwrap_or(DETAIL_EPISODE_SCROLL_STEP_PX);
        self.set_series_episodes_scroll_offset(offset, window, cx);
    }

    fn set_series_episodes_scroll_offset(
        &mut self,
        offset: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let viewport_width = home_main_content_width(window);
        let max_offset = self
            .series_detail
            .as_ref()
            .and_then(|detail| detail.episodes.as_ref())
            .map(|episodes| {
                max_carousel_scroll_offset_for(
                    episodes.items.len(),
                    viewport_width,
                    DETAIL_EPISODE_CARD_WIDTH_PX,
                    DETAIL_EPISODE_CARD_PADDING_PX,
                    DETAIL_EPISODE_CARD_GAP_PX,
                )
            })
            .unwrap_or(0.0);
        let Some(detail) = &mut self.series_detail else {
            return;
        };

        if detail
            .episodes_carousel
            .set_scroll_offset(offset, max_offset)
        {
            cx.notify();
        }
    }

    pub(super) fn set_series_people_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        let Some(detail) = &mut self.series_detail else {
            return;
        };
        if detail.people_carousel.set_hovered(hovered) {
            cx.notify();
        }
    }

    pub(super) fn set_series_people_controls_hovered(
        &mut self,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = &mut self.series_detail else {
            return;
        };
        if detail.people_carousel.set_controls_hovered(hovered) {
            cx.notify();
        }
    }

    pub(super) fn scroll_series_people_left(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self
            .series_detail
            .as_ref()
            .map(|detail| {
                detail.people_carousel.scroll_offset(f32::INFINITY) - DETAIL_PERSON_SCROLL_STEP_PX
            })
            .unwrap_or(0.0);
        self.set_series_people_scroll_offset(offset, window, cx);
    }

    pub(super) fn scroll_series_people_right(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self
            .series_detail
            .as_ref()
            .map(|detail| {
                detail.people_carousel.scroll_offset(f32::INFINITY) + DETAIL_PERSON_SCROLL_STEP_PX
            })
            .unwrap_or(DETAIL_PERSON_SCROLL_STEP_PX);
        self.set_series_people_scroll_offset(offset, window, cx);
    }

    fn set_series_people_scroll_offset(
        &mut self,
        offset: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let viewport_width = home_main_content_width(window);
        let max_offset = self
            .series_detail
            .as_ref()
            .and_then(|detail| detail.item.as_ref())
            .and_then(|item| item.people.as_ref())
            .map(|people| {
                max_carousel_scroll_offset_for(
                    people.len(),
                    viewport_width,
                    DETAIL_PERSON_CARD_WIDTH_PX,
                    DETAIL_PERSON_CARD_PADDING_PX,
                    DETAIL_PERSON_CARD_GAP_PX,
                )
            })
            .unwrap_or(0.0);
        let Some(detail) = &mut self.series_detail else {
            return;
        };

        if detail.people_carousel.set_scroll_offset(offset, max_offset) {
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_range_returns_empty_for_empty_carousel() {
        assert_eq!(
            carousel_visible_range_for(0, 0.0, 20.0, 10.0, 0.0, 2.0, (1, 1)),
            CarouselVisibleRange {
                start: 0,
                end: 0,
                leading_width: 0.0,
                trailing_width: 0.0,
            }
        );
    }

    #[test]
    fn visible_range_renders_all_items_when_they_fit() {
        assert_eq!(
            carousel_visible_range_for(5, 0.0, 100.0, 10.0, 0.0, 2.0, (1, 1)),
            CarouselVisibleRange {
                start: 0,
                end: 5,
                leading_width: 0.0,
                trailing_width: 0.0,
            }
        );
    }

    #[test]
    fn visible_range_adds_overscan_and_spacer_widths() {
        assert_eq!(
            carousel_visible_range_for(10, 36.0, 25.0, 10.0, 0.0, 2.0, (1, 2)),
            CarouselVisibleRange {
                start: 2,
                end: 8,
                leading_width: 22.0,
                trailing_width: 22.0,
            }
        );
    }

    #[test]
    fn visible_range_between_keeps_previous_cards_during_animation() {
        assert_eq!(
            carousel_visible_range_between_for(10, (0.0, 36.0), 25.0, 10.0, 0.0, 2.0, (1, 2)),
            CarouselVisibleRange {
                start: 0,
                end: 8,
                leading_width: 0.0,
                trailing_width: 22.0,
            }
        );
    }

    #[test]
    fn visible_range_clamps_to_the_end() {
        assert_eq!(
            carousel_visible_range_for(5, 100.0, 20.0, 10.0, 0.0, 2.0, (1, 2)),
            CarouselVisibleRange {
                start: 2,
                end: 5,
                leading_width: 22.0,
                trailing_width: 0.0,
            }
        );
    }
}
