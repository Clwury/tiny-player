use super::*;
use gpui::{Render, TestAppContext};

struct Host {
    dropdown: Option<Entity<DropdownState>>,
    outside: FocusHandle,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .track_focus(&self.outside)
            .when_some(self.dropdown.as_ref(), |view, dropdown| {
                view.child(div().size(px(20.0)).track_focus(&dropdown.read(cx).focus))
            })
    }
}

#[gpui::test]
fn reopening_the_same_menu_waits_for_its_own_two_frames(cx: &mut TestAppContext) {
    let (host, cx) = cx.add_window_view(|_, cx| Host {
        dropdown: Some(cx.new(DropdownState::new)),
        outside: cx.focus_handle(),
    });
    cx.run_until_parked();
    let (dropdown, outside) = host.read_with(cx, |host, _| {
        (host.dropdown.clone().unwrap(), host.outside.clone())
    });
    cx.update(|window, cx| {
        window.focus(&outside, cx);
        let retired = dropdown.update(cx, |dropdown, cx| {
            dropdown.show("menu", 0, window, cx);
            dropdown.pending_focus.latest().unwrap()
        });
        window.simulate_next_frame(cx);
        dropdown.update(cx, |dropdown, cx| {
            dropdown.close(cx);
            dropdown.show("menu", 1, window, cx);
        });
        // The retired opening's second callback and the replacement's first
        // callback run here. The retired callback must not steal focus early.
        window.simulate_next_frame(cx);
        assert!(outside.is_focused(window));
        window.simulate_next_frame(cx);
        assert!(dropdown.read(cx).focus.is_focused(window));
        assert!(!dropdown.read(cx).pending_focus.is_active());
        window.focus(&outside, cx);
        dropdown.update(cx, |dropdown, cx| {
            dropdown.finish_focus("menu", &retired, window, cx)
        });
        assert!(outside.is_focused(window));
    });
}

#[gpui::test]
fn closing_or_releasing_a_menu_prevents_deferred_focus(cx: &mut TestAppContext) {
    let (host, cx) = cx.add_window_view(|_, cx| Host {
        dropdown: Some(cx.new(DropdownState::new)),
        outside: cx.focus_handle(),
    });
    cx.run_until_parked();
    let (dropdown, outside) = host.read_with(cx, |host, _| {
        (host.dropdown.clone().unwrap(), host.outside.clone())
    });
    cx.update(|window, cx| {
        window.focus(&outside, cx);
        dropdown.update(cx, |dropdown, cx| {
            dropdown.show("menu", 0, window, cx);
            dropdown.close(cx);
        });
        window.simulate_next_frame(cx);
        window.simulate_next_frame(cx);
        assert!(outside.is_focused(window));
        dropdown.update(cx, |dropdown, cx| dropdown.show("menu", 0, window, cx));
    });
    let weak = dropdown.downgrade();
    host.update(cx, |host, _| host.dropdown = None);
    cx.update(|_, _| drop(dropdown));
    cx.run_until_parked();
    cx.update(|window, cx| {
        assert!(weak.upgrade().is_none());
        window.simulate_next_frame(cx);
        window.simulate_next_frame(cx);
        assert!(outside.is_focused(window));
    });
}
