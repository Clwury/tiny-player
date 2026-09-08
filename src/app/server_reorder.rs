use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{Context, FocusHandle, IntoElement, Pixels, Point, Styled, Window, canvas, px};

use crate::server::CachedServer;

use super::{Page, TinyApp};

const REORDER_DURATION: Duration = Duration::from_millis(180);

pub(super) struct ServerReorder {
    pub(super) server_id: String,
    target_index: usize,
    pub(super) focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
}

#[derive(Clone, Default)]
pub(super) struct CardPosition(Rc<Cell<Option<(usize, CardMotion)>>>);

#[derive(Clone, Copy)]
struct CardMotion {
    from: Point<Pixels>,
    target: Point<Pixels>,
    started: Instant,
}

impl CardMotion {
    fn position(self, now: Instant) -> Point<Pixels> {
        let progress = (now.saturating_duration_since(self.started).as_secs_f32()
            / REORDER_DURATION.as_secs_f32())
        .min(1.0);
        let eased = 1.0 - (1.0 - progress).powi(3);
        self.from + (self.target - self.from) * eased
    }

    fn retarget(&mut self, target: Point<Pixels>, now: Instant) {
        if self.target != target {
            self.from = self.position(now);
            self.target = target;
            self.started = now;
        }
    }
}

/// Animate the card within its final layout slot. The slot's drag hitbox stays
/// stationary so cards sliding under the pointer cannot repeatedly reorder it.
pub(super) fn animated_card(
    mut card: gpui::AnyElement,
    position: CardPosition,
    slot_index: usize,
    placeholder: bool,
) -> impl IntoElement {
    canvas(
        move |bounds, window, cx| {
            let now = cx.background_executor().now();
            let (previous_index, mut motion) = position.0.get().unwrap_or((
                slot_index,
                CardMotion {
                    from: bounds.origin,
                    target: bounds.origin,
                    started: now,
                },
            ));
            // Follow window resizing directly; only a changed order starts a transition.
            if placeholder
                || cx.reduce_motion()
                || (previous_index == slot_index && motion.target != bounds.origin)
            {
                motion.from = bounds.origin;
                motion.target = bounds.origin;
            } else {
                motion.retarget(bounds.origin, now);
            }
            let origin = motion.position(now);
            position.0.set(Some((slot_index, motion)));
            if origin != motion.target {
                window.request_animation_frame();
            }
            card.layout_as_root(bounds.size.into(), window, cx);
            card.prepaint_at(origin, window, cx);
            card
        },
        |_, mut card, window, cx| card.paint(window, cx),
    )
    .w(px(super::server_card::SERVER_CARD_WIDTH_PX))
    .h(px(super::server_card::SERVER_CARD_HEIGHT_PX))
}

impl TinyApp {
    pub(super) fn begin_server_reorder(
        &mut self,
        server_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.page, Page::Servers)
            || self.selecting_server_id.is_some()
            || self.add_server_dialog.is_some()
        {
            return;
        }
        let Some(target_index) = self
            .servers
            .iter()
            .position(|server| server.id == server_id)
        else {
            return;
        };
        self.dismiss_server_menu(window, cx);
        let focus = cx.focus_handle();
        let previous_focus = window.focused(cx);
        focus.focus(window, cx);
        self.server_reorder = Some(ServerReorder {
            server_id: server_id.into(),
            target_index,
            focus,
            previous_focus,
        });
        cx.notify();
    }

    pub(super) fn preview_server_reorder(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(reorder) = &mut self.server_reorder
            && index < self.servers.len()
            && reorder.target_index != index
        {
            reorder.target_index = index;
            cx.notify();
        }
    }

    pub(super) fn preview_servers(&self) -> Vec<CachedServer> {
        let mut servers = self.servers.clone();
        if let Some(reorder) = &self.server_reorder
            && let Some(source) = servers
                .iter()
                .position(|server| server.id == reorder.server_id)
        {
            let target = reorder.target_index.min(servers.len() - 1);
            let server = servers.remove(source);
            servers.insert(target, server);
        }
        servers
    }

    pub(super) fn finish_server_reorder(
        &mut self,
        commit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(reorder) = self.server_reorder.take() else {
            return;
        };
        if commit && let Some(target) = self.cache.servers.get(reorder.target_index) {
            let target_id = target.id.clone();
            self.reorder_server(&reorder.server_id, &target_id, cx);
        }
        if reorder.focus.is_focused(window) {
            if let Some(focus) = reorder.previous_focus {
                focus.focus(window, cx);
            } else {
                window.blur(cx);
            }
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::point;

    #[test]
    fn changing_direction_mid_animation_preserves_the_current_position() {
        let started = Instant::now();
        let mut motion = CardMotion {
            from: point(px(0.0), px(0.0)),
            target: point(px(232.0), px(122.0)),
            started,
        };
        let halfway = started + REORDER_DURATION / 2;
        let current = motion.position(halfway);
        assert!(current.x > px(0.0) && current.x < px(232.0));
        let target = point(px(464.0), px(0.0));
        motion.retarget(target, halfway);
        assert_eq!(motion.position(halfway), current);
        assert_eq!(motion.position(halfway + REORDER_DURATION), target);
        // Hovering the same destination does not restart an in-flight transition.
        motion.retarget(target, halfway + REORDER_DURATION / 2);
        assert_eq!(motion.started, halfway);
    }
}
