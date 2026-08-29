use std::{collections::VecDeque, time::Duration};

use gpui::{
    Animation, AnimationExt as _, App, ClickEvent, InteractiveElement, IntoElement, MouseButton,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, div, ease_in_out, px,
    svg,
};

use crate::theme;

/// The default amount of time an automatically managed notification remains visible.
pub(crate) const NOTIFICATION_AUTOHIDE: Duration = Duration::from_secs(5);

const NOTIFICATION_MAX_ITEMS: usize = 10;
const NOTIFICATION_WIDTH_PX: f32 = 448.0;

#[derive(Clone, Debug)]
pub(crate) struct Notification {
    pub(crate) id: u64,
    pub(crate) message: SharedString,
}

#[derive(Clone, Debug)]
pub(crate) struct NotificationEntry<K> {
    pub(crate) key: K,
    pub(crate) notification: Notification,
}

/// A small, key-addressable queue shared by pages that need transient errors.
///
/// Reusing a key replaces the previous message, which prevents a retry from
/// leaving stale copies of the same error in the notification stack.
#[derive(Clone, Debug)]
pub(crate) struct NotificationQueue<K> {
    items: VecDeque<NotificationEntry<K>>,
    next_id: u64,
}

impl<K> Default for NotificationQueue<K> {
    fn default() -> Self {
        Self {
            items: VecDeque::new(),
            next_id: 0,
        }
    }
}

impl<K: PartialEq> NotificationQueue<K> {
    pub(crate) fn push(&mut self, key: K, message: SharedString) -> u64 {
        self.items.retain(|entry| entry.key != key);
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        self.items.push_back(NotificationEntry {
            key,
            notification: Notification { id, message },
        });
        while self.items.len() > NOTIFICATION_MAX_ITEMS {
            self.items.pop_front();
        }
        id
    }

    pub(crate) fn remove(&mut self, id: u64) -> bool {
        let previous_len = self.items.len();
        self.items.retain(|entry| entry.notification.id != id);
        self.items.len() != previous_len
    }

    pub(crate) fn retain(&mut self, f: impl FnMut(&NotificationEntry<K>) -> bool) {
        self.items.retain(f);
    }
}

impl<K> NotificationQueue<K> {
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &NotificationEntry<K>> {
        self.items.iter()
    }

    pub(crate) fn clear(&mut self) {
        self.items.clear();
    }
}

/// Render the shared top-right notification stack.
pub(crate) fn notification_layer() -> gpui::Div {
    div()
        .absolute()
        .top_4()
        .right_4()
        .flex()
        .w(px(NOTIFICATION_WIDTH_PX))
        .max_w_full()
        .flex_col()
        .items_end()
        .gap_3()
}

/// Render one error notification card.
pub(crate) fn error_notification(
    id: u64,
    message: SharedString,
    on_dismiss: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .id(("notification", id))
        .relative()
        .flex()
        .w_full()
        .gap_3()
        .occlude()
        .rounded(theme.radius_lg)
        .border_1()
        .border_color(theme.input_border)
        .bg(theme.dialog_background)
        .shadow_lg()
        .px_4()
        .py(px(14.0))
        .pr_10()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(
            div().absolute().top(px(18.0)).left_4().child(
                svg()
                    .path("icons/circle-x.svg")
                    .size(px(16.0))
                    .text_color(theme.error),
            ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .flex_1()
                .overflow_hidden()
                .pl_6()
                .text_sm()
                .whitespace_normal()
                .text_color(theme.foreground)
                .child(message),
        )
        .child(
            div()
                .id(("notification-close", id))
                .absolute()
                .top_2()
                .right_2()
                .flex()
                .size(px(22.0))
                .items_center()
                .justify_center()
                .rounded_md()
                .text_color(theme.muted_foreground)
                .hover(move |style| style.bg(theme.secondary_hover).text_color(theme.foreground))
                .cursor_pointer()
                .child(
                    svg()
                        .path("icons/window-close.svg")
                        .size(px(12.0))
                        .text_color(theme.muted_foreground),
                )
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .on_click(on_dismiss),
        )
        .with_animation(
            ("notification-appear", id),
            Animation::new(Duration::from_millis(250)).with_easing(ease_in_out),
            |this, delta| this.top(px(-45.0) + delta * px(45.0)).opacity(delta),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushing_same_key_replaces_the_previous_notification() {
        let mut queue = NotificationQueue::default();
        let first = queue.push("home", "first".into());
        let second = queue.push("home", "second".into());

        assert_ne!(first, second);
        assert_eq!(queue.iter().count(), 1);
        assert_eq!(queue.iter().next().unwrap().notification.message, "second");
    }

    #[test]
    fn removing_notification_by_id_reports_whether_it_existed() {
        let mut queue = NotificationQueue::default();
        let id = queue.push("home", "error".into());

        assert!(queue.remove(id));
        assert!(!queue.remove(id));
        assert!(queue.is_empty());
    }

    #[test]
    fn queue_keeps_only_the_most_recent_notifications() {
        let mut queue = NotificationQueue::default();
        for index in 0..(NOTIFICATION_MAX_ITEMS + 2) {
            queue.push(index, format!("error-{index}").into());
        }

        let retained = queue.iter().map(|entry| entry.key).collect::<Vec<_>>();
        assert_eq!(
            retained,
            (2..(NOTIFICATION_MAX_ITEMS + 2)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn clearing_queue_keeps_ids_monotonic_for_old_autohide_timers() {
        let mut queue = NotificationQueue::default();
        let old_id = queue.push("old", "old error".into());
        queue.clear();
        let new_id = queue.push("new", "new error".into());

        assert!(new_id > old_id);
        assert!(!queue.remove(old_id));
        assert_eq!(queue.iter().next().unwrap().notification.id, new_id);
    }
}
