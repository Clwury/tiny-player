use gpui::{IntoElement, Pixels, Point, Styled, canvas, px};
use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

const REORDER_DURATION: Duration = Duration::from_millis(180);

#[derive(Clone, Default)]
pub(crate) struct CardPosition(Rc<Cell<Option<(usize, CardMotion)>>>);

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
pub(crate) fn animated_card(
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
    .w(px(super::SERVER_CARD_WIDTH_PX))
    .h(px(super::SERVER_CARD_HEIGHT_PX))
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
