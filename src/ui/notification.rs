use std::{collections::VecDeque, time::Duration};

use gpui::{
    Animation, AnimationExt as _, App, ClickEvent, Context, InteractiveElement, IntoElement,
    MouseButton, ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, div,
    ease_in_out, px, svg,
};

use crate::effects::{EffectHandle, RequestScope, RequestSlot, RequestToken, WorkspaceIdentity};
use crate::theme;
use crate::ui::radius;

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
/// The host owns the queue; identity stays fixed until that host is replaced.
/// Only these queue operations write entries and their bounded deadline tasks.
#[derive(Debug)]
pub(crate) struct NotificationQueue<K> {
    items: VecDeque<ManagedNotification<K>>,
    next_id: u64,
    identity: WorkspaceIdentity,
}

/// Presentation-owned deadline: push_autohide issues its token; replacing, dismissing,
/// filtering, evicting, clearing or dropping the entry invalidates it and drops
/// the task. Local timers have no IO errors or error-notification key.
#[derive(Debug)]
struct ManagedNotification<K> {
    entry: NotificationEntry<K>,
    deadline: RequestSlot,
    task: EffectHandle<gpui::Task<()>>,
}

impl<K> Default for NotificationQueue<K> {
    fn default() -> Self {
        Self::new(WorkspaceIdentity::default())
    }
}

impl<K> NotificationQueue<K> {
    /// Home supplies its immutable account identity. App/dialog queues use the
    /// local identity; each entry's slot also fences callbacks by unique owner.
    pub(crate) fn new(identity: WorkspaceIdentity) -> Self {
        Self {
            items: VecDeque::new(),
            next_id: 0,
            identity,
        }
    }
}

impl<K: PartialEq> NotificationQueue<K> {
    pub(crate) fn push(&mut self, key: K, message: SharedString) -> u64 {
        self.retain(|entry| entry.key != key);
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let deadline =
            RequestSlot::new(RequestScope::NotificationHide { id }, self.identity.clone());
        self.items.push_back(ManagedNotification {
            entry: NotificationEntry {
                key,
                notification: Notification { id, message },
            },
            deadline,
            task: EffectHandle::default(),
        });
        while self.items.len() > NOTIFICATION_MAX_ITEMS {
            self.items.pop_front();
        }
        id
    }

    /// Shared GPUI runner; the accessor is the only connection to the host page.
    pub(crate) fn push_autohide<T: 'static>(
        &mut self,
        key: K,
        message: SharedString,
        cx: &mut Context<T>,
        queue: fn(&mut T) -> &mut Self,
    ) where
        K: 'static,
    {
        let id = self.push(key, message);
        let item = self.items.back_mut().expect("just pushed notification");
        let token = item.deadline.issue();
        item.task.replace(cx.spawn(async move |owner, cx| {
            cx.background_executor().timer(NOTIFICATION_AUTOHIDE).await;
            owner
                .update(cx, |owner, cx| {
                    if queue(owner).expire(id, &token) {
                        cx.notify();
                    }
                })
                .ok();
        }));
        cx.notify();
    }

    fn expire(&mut self, id: u64, token: &RequestToken) -> bool {
        if !token.is_for(&self.identity) {
            return false;
        }
        let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.entry.notification.id == id)
        else {
            return false;
        };
        if !item.deadline.commit(token) {
            return false;
        }
        self.remove(id)
    }

    pub(crate) fn remove(&mut self, id: u64) -> bool {
        let previous_len = self.items.len();
        self.retain(|entry| entry.notification.id != id);
        self.items.len() != previous_len
    }

    pub(crate) fn retain(&mut self, mut f: impl FnMut(&NotificationEntry<K>) -> bool) {
        self.items.retain(|item| f(&item.entry));
    }
}

impl<K> NotificationQueue<K> {
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &NotificationEntry<K>> {
        self.items.iter().map(|item| &item.entry)
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
        .cursor_default()
        .rounded(radius::SURFACE)
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
                .rounded(radius::CONTROL)
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
mod tests;
