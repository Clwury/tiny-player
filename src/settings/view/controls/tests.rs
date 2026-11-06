use std::{cell::RefCell, path::PathBuf, rc::Rc};

use gpui::{Modifiers, Render, TestAppContext, Window};

use super::*;

struct LogPathView {
    path: Option<PathBuf>,
    revealed: Rc<RefCell<Vec<PathBuf>>>,
}

impl Render for LogPathView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let revealed = Rc::clone(&self.revealed);
        div().size_full().child(playback_log_path_field(
            self.path.clone(),
            move |path, _| revealed.borrow_mut().push(path.to_path_buf()),
            cx,
        ))
    }
}

#[gpui::test]
fn log_path_uses_an_input_frame_and_reveals_the_complete_path_on_click(cx: &mut TestAppContext) {
    cx.update(theme::init);
    let path = PathBuf::from("日志目录 with spaces/playback-debug.log");
    let revealed = Rc::new(RefCell::new(Vec::new()));
    let (_, cx) = cx.add_window_view(|_, _| LogPathView {
        path: Some(path.clone()),
        revealed: Rc::clone(&revealed),
    });
    let bounds = cx.debug_bounds("playback-debug-log-path").unwrap();
    assert_eq!(bounds.size, gpui::size(px(256.0), px(32.0)));
    cx.update(|window, cx| {
        assert!(window.painted_quads().iter().any(|quad| {
            quad.bounds == bounds.scale(window.scale_factor())
                && quad.background == theme::get(cx).editor_background.into()
        }));
    });
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(*revealed.borrow(), vec![path]);
    assert_eq!(
        cx.opened_url(),
        None,
        "reveal the file instead of opening it as a URL"
    );
}

#[gpui::test]
fn unavailable_log_paths_do_not_invoke_the_file_manager(cx: &mut TestAppContext) {
    cx.update(theme::init);
    let revealed = Rc::new(RefCell::new(Vec::new()));
    let (_, cx) = cx.add_window_view(|_, _| LogPathView {
        path: None,
        revealed: Rc::clone(&revealed),
    });
    let bounds = cx.debug_bounds("playback-debug-log-path").unwrap();
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(revealed.borrow().is_empty());
}
