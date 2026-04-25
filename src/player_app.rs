use crate::{
    media::discover_playlist,
    mpv_backend::{BackendEvent, MpvBackend},
    sample_dir,
    state::AppState,
    video_presenter::VideoPresenter,
};
use gpui::{
    App, Application, Bounds, Context, Pixels, Render, StatefulInteractiveElement, Timer, Window,
    WindowBounds, WindowOptions, canvas, div, img, prelude::*, px, rgb, size,
};
use std::time::Duration;

fn selection_changed(before: Option<usize>, after: Option<usize>) -> bool {
    before != after
}

fn reset_progress(state: &mut AppState) {
    state.update_progress(0.0, 0.0);
}

fn handle_backend_failure(state: &mut AppState, message: impl Into<String>) {
    reset_progress(state);
    state.set_error(message);
}

fn handle_render_failure(
    state: &mut AppState,
    status_message: &mut String,
    message: impl Into<String>,
) {
    reset_progress(state);
    *status_message = message.into();
}

struct ShutdownOrder<Owner, Dependent> {
    owner: Option<Owner>,
    dependent: Option<Dependent>,
}

impl<Owner, Dependent> ShutdownOrder<Owner, Dependent> {
    fn new(owner: Option<Owner>, dependent: Option<Dependent>) -> Self {
        Self { owner, dependent }
    }

    fn owner(&self) -> Option<&Owner> {
        self.owner.as_ref()
    }

    fn owner_mut(&mut self) -> Option<&mut Owner> {
        self.owner.as_mut()
    }

    fn dependent(&self) -> Option<&Dependent> {
        self.dependent.as_ref()
    }

    fn dependent_mut(&mut self) -> Option<&mut Dependent> {
        self.dependent.as_mut()
    }
}

impl<Owner, Dependent> Drop for ShutdownOrder<Owner, Dependent> {
    fn drop(&mut self) {
        // The render context must be freed before the backing mpv handle.
        drop(self.dependent.take());
        drop(self.owner.take());
    }
}

fn handle_file_title_update(
    current_title: &mut String,
    state: &mut AppState,
    status_message: &mut String,
    title: String,
) {
    *current_title = title;
    state.clear_error();
    status_message.clear();
}

fn normalize_video_viewport(bounds: Bounds<Pixels>) -> Option<(u32, u32)> {
    let width = f32::from(bounds.size.width).floor().max(0.0) as u32;
    let height = f32::from(bounds.size.height).floor().max(0.0) as u32;

    (width > 0 && height > 0).then_some((width, height))
}

fn viewport_changed(previous: Option<Bounds<Pixels>>, next: Bounds<Pixels>) -> bool {
    previous != Some(next)
}

fn should_render_frame(
    has_video_presenter: bool,
    has_current_entry: bool,
    has_error: bool,
    has_viewport: bool,
) -> bool {
    has_video_presenter && has_current_entry && !has_error && has_viewport
}

fn can_toggle_playback(has_backend: bool, can_control_playback: bool, has_error: bool) -> bool {
    has_backend && can_control_playback && !has_error
}

fn overlay_progress_fill_fraction(progress_fraction: f32) -> f32 {
    progress_fraction.clamp(0.0, 1.0)
}

fn overlay_progress_bottom_inset_px() -> f32 {
    12.0
}

fn overlay_progress_horizontal_inset_px() -> f32 {
    14.0
}

fn bottom_control_labels() -> [&'static str; 3] {
    ["Previous", "Play/Pause", "Next"]
}

#[derive(Clone, Copy)]
enum Direction {
    Previous,
    Next,
}

fn adjacent_index(
    selected_index: Option<usize>,
    playlist_len: usize,
    direction: Direction,
) -> Option<usize> {
    let index = selected_index?;

    match direction {
        Direction::Previous => index.checked_sub(1),
        Direction::Next => index
            .checked_add(1)
            .filter(|&next_index| next_index < playlist_len),
    }
}

pub fn run() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1100.0), px(720.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| PlayerApp::new()),
        )
        .unwrap();
        cx.activate(true);
    });
}

struct PlayerApp {
    state: AppState,
    video: ShutdownOrder<MpvBackend, VideoPresenter>,
    video_viewport_bounds: Option<Bounds<Pixels>>,
    current_title: String,
    status_message: String,
    polling_started: bool,
    awaiting_initial_frame: bool,
    current_file_loaded: bool,
}

impl PlayerApp {
    fn new() -> Self {
        let (playlist, mut status_message) = match discover_playlist(&sample_dir()) {
            Ok(playlist) if playlist.is_empty() => {
                (playlist, "No videos found in sample/".to_string())
            }
            Ok(playlist) => (playlist, String::new()),
            Err(error) => (Vec::new(), format!("Failed to read sample/: {error}")),
        };
        let mut state = AppState::from_playlist(playlist);
        let current_title = state.current_title().to_owned();

        let (backend, video_presenter) = match MpvBackend::new() {
            Ok(mut backend) => match VideoPresenter::new(backend.mpv_mut()) {
                Ok(video_presenter) => {
                    if let Some((path, display_name)) = state
                        .current_entry()
                        .map(|entry| (entry.path.clone(), entry.display_name.clone()))
                    {
                        if let Err(error) = backend.load_file(&path) {
                            state.set_error(error.to_string());
                            status_message = format!("Failed to preload {display_name}");
                        }
                    }
                    (Some(backend), Some(video_presenter))
                }
                Err(error) => {
                    state.set_error(error.to_string());
                    status_message = "Failed to create video presenter".to_string();
                    (Some(backend), None)
                }
            },
            Err(error) => {
                state.set_error(error.to_string());
                status_message = "Failed to create mpv backend".to_string();
                (None, None)
            }
        };

        let awaiting_initial_frame = state.current_entry().is_some() && backend.is_some();

        Self {
            state,
            video: ShutdownOrder::new(backend, video_presenter),
            video_viewport_bounds: None,
            current_title,
            status_message,
            polling_started: false,
            awaiting_initial_frame,
            current_file_loaded: false,
        }
    }

    fn ensure_polling(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.polling_started {
            return;
        }

        self.polling_started = true;
        let view = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                loop {
                    Timer::after(Duration::from_millis(33)).await;
                    if view
                        .update_in(&mut *cx, |this, window, cx| this.poll_backend(window, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
    }

    fn clear_visible_frame(&mut self, window: &mut Window) {
        if let Some(video_presenter) = self.video.dependent_mut() {
            video_presenter.clear_frame(window);
        }
    }

    fn update_video_viewport(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        if !viewport_changed(self.video_viewport_bounds.clone(), bounds.clone()) {
            return;
        }

        self.video_viewport_bounds = Some(bounds);
        cx.notify();
    }

    fn poll_backend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut should_notify = false;

        if let Some(backend) = self.video.owner_mut() {
            for event in backend.poll_events() {
                match event {
                    BackendEvent::Pause(paused) => {
                        if self.awaiting_initial_frame {
                            self.state.is_playing = false;
                        } else {
                            self.state.sync_pause_state(paused);
                        }
                        should_notify = true;
                    }
                    BackendEvent::PlaybackRestart => {
                        self.current_file_loaded = true;
                        should_notify = true;
                    }
                    BackendEvent::FileTitle(title) => {
                        handle_file_title_update(
                            &mut self.current_title,
                            &mut self.state,
                            &mut self.status_message,
                            title,
                        );
                        should_notify = true;
                    }
                    BackendEvent::PositionChanged(position) => {
                        self.state.update_position(position);
                        should_notify = true;
                    }
                    BackendEvent::DurationChanged(duration) => {
                        self.state.update_duration(duration);
                        should_notify = true;
                    }
                    BackendEvent::LoadFailed(message) => {
                        self.clear_visible_frame(window);
                        handle_backend_failure(&mut self.state, message);
                        should_notify = true;
                    }
                    BackendEvent::Fatal(message) => {
                        self.clear_visible_frame(window);
                        handle_backend_failure(&mut self.state, message);
                        should_notify = true;
                    }
                }
            }
        }

        let viewport = self
            .video_viewport_bounds
            .clone()
            .and_then(normalize_video_viewport);

        if should_render_frame(
            self.video.dependent().is_some(),
            self.current_file_loaded,
            self.state.error_message.is_some(),
            viewport.is_some(),
        ) {
            let (width, height) = viewport.expect("viewport checked above");
            let render_result = self
                .video
                .dependent_mut()
                .expect("video presenter checked above")
                .render_if_needed(width, height, window);

            match render_result {
                Ok(true) => {
                    self.status_message.clear();
                    if self.awaiting_initial_frame {
                        self.awaiting_initial_frame = false;
                        if let Some(backend) = self.video.owner_mut() {
                            if let Err(error) = backend.pause() {
                                self.clear_visible_frame(window);
                                handle_backend_failure(&mut self.state, error.to_string());
                            }
                        }
                        self.state.sync_pause_state(true);
                    }
                    should_notify = true;
                }
                Ok(false) => {}
                Err(error) => {
                    self.clear_visible_frame(window);
                    handle_render_failure(
                        &mut self.state,
                        &mut self.status_message,
                        error.to_string(),
                    );
                    should_notify = true;
                }
            }
        }

        if should_notify {
            cx.notify();
        }
    }

    fn select_playlist_item(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let previous_selection = self.state.selected_index;
        self.state.select(index);
        if !selection_changed(previous_selection, self.state.selected_index) {
            return;
        }

        self.clear_visible_frame(window);
        reset_progress(&mut self.state);
        self.awaiting_initial_frame = false;
        self.current_file_loaded = false;
        self.current_title = self.state.current_title().to_owned();

        let Some(entry) = self.state.current_entry() else {
            return;
        };

        if let Some(backend) = self.video.owner_mut() {
            match backend.load_file(&entry.path) {
                Ok(()) => {
                    self.awaiting_initial_frame = true;
                }
                Err(error) => {
                    self.clear_visible_frame(window);
                    handle_backend_failure(&mut self.state, error.to_string());
                }
            }
        }

        cx.notify();
    }

    fn select_adjacent_item(
        &mut self,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = adjacent_index(
            self.state.selected_index,
            self.state.playlist.len(),
            direction,
        ) else {
            return;
        };

        self.select_playlist_item(index, window, cx);
    }

    fn toggle_playback(&mut self, cx: &mut Context<Self>) {
        if !can_toggle_playback(
            self.video.owner().is_some(),
            self.state.can_control_playback(),
            self.state.error_message.is_some(),
        ) {
            return;
        }

        let Some(backend) = self.video.owner_mut() else {
            return;
        };

        match backend.toggle_playback() {
            Ok(is_playing) => {
                self.state.is_playing = is_playing;
                self.state.clear_error();
            }
            Err(error) => self.state.set_error(error.to_string()),
        }

        cx.notify();
    }

    fn message_text(&self) -> &str {
        self.state
            .error_message
            .as_deref()
            .unwrap_or(self.status_message.as_str())
    }
}

impl Render for PlayerApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_polling(window, cx);

        let playlist = self.state.playlist.iter().enumerate().fold(
            div().flex().flex_col().gap_1(),
            |playlist, (index, entry)| {
                let selected = self.state.selected_index == Some(index);
                playlist.child(
                    div()
                        .id(("playlist-item", index))
                        .px_3()
                        .py_2()
                        .rounded_sm()
                        .cursor_pointer()
                        .bg(if selected {
                            rgb(0x2f3a4a)
                        } else {
                            rgb(0x20242b)
                        })
                        .text_color(rgb(0xf5f7fa))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select_playlist_item(index, window, cx);
                        }))
                        .child(entry.display_name.clone()),
                )
            },
        );

        let play_label = if self.state.is_playing {
            "Pause"
        } else {
            "Play"
        };
        let can_go_previous = self.state.has_previous();
        let can_go_next = self.state.has_next();
        let can_toggle_playback = can_toggle_playback(
            self.video.owner().is_some(),
            self.state.can_control_playback(),
            self.state.error_message.is_some(),
        );
        let [previous_label, _, next_label] = bottom_control_labels();
        let progress_fill_fraction = overlay_progress_fill_fraction(self.state.progress_fraction());
        let message_text = self.message_text().to_string();
        let current_frame = self
            .video
            .dependent()
            .and_then(|video_presenter| video_presenter.current_frame());
        let view = cx.entity().downgrade();
        let viewport_observer = canvas(
            |bounds, _, _| bounds,
            move |_bounds, observed_bounds, window, _app| {
                let view = view.clone();
                let observed_bounds = observed_bounds.clone();
                window.on_next_frame(move |_, app| {
                    let _ = view.update(app, |this, cx| {
                        this.update_video_viewport(observed_bounds, cx);
                    });
                });
            },
        )
        .absolute()
        .size_full();

        let video_area = if let Some(frame) = current_frame {
            div()
                .flex()
                .flex_1()
                .bg(rgb(0x050505))
                .child(img(frame).size_full())
        } else {
            div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .bg(rgb(0x050505))
                .text_color(rgb(0x9aa5b1))
                .child("Video output will appear here")
        };

        let mut playback_toggle = div()
            .id("playback-toggle")
            .px_4()
            .py_2()
            .rounded_sm()
            .bg(if can_toggle_playback {
                rgb(0x238636)
            } else {
                rgb(0x3a434d)
            })
            .child(play_label);

        playback_toggle = if can_toggle_playback {
            playback_toggle
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.toggle_playback(cx);
                }))
        } else {
            playback_toggle.cursor_default()
        };

        let button_bg = |enabled| {
            if enabled {
                rgb(0x30363d)
            } else {
                rgb(0x1f242d)
            }
        };
        let button_text = |enabled| {
            if enabled {
                rgb(0xe6edf3)
            } else {
                rgb(0x6e7681)
            }
        };

        let mut previous_button = div()
            .id("previous-button")
            .px_4()
            .py_2()
            .rounded_sm()
            .bg(button_bg(can_go_previous))
            .text_color(button_text(can_go_previous))
            .child(previous_label);

        previous_button = if can_go_previous {
            previous_button
                .cursor_pointer()
                .on_click(cx.listener(|this, _, window, cx| {
                    this.select_adjacent_item(Direction::Previous, window, cx);
                }))
        } else {
            previous_button.cursor_default()
        };

        let mut next_button = div()
            .id("next-button")
            .px_4()
            .py_2()
            .rounded_sm()
            .bg(button_bg(can_go_next))
            .text_color(button_text(can_go_next))
            .child(next_label);

        next_button = if can_go_next {
            next_button
                .cursor_pointer()
                .on_click(cx.listener(|this, _, window, cx| {
                    this.select_adjacent_item(Direction::Next, window, cx);
                }))
        } else {
            next_button.cursor_default()
        };

        let overlay_progress_bar = div()
            .absolute()
            .left(px(overlay_progress_horizontal_inset_px()))
            .right(px(overlay_progress_horizontal_inset_px()))
            .bottom(px(overlay_progress_bottom_inset_px()))
            .h(px(8.0))
            .rounded_sm()
            .bg(gpui::rgba(0x00000073))
            .child(
                div()
                    .h_full()
                    .w(gpui::relative(progress_fill_fraction))
                    .rounded_sm()
                    .bg(rgb(0x2f81f7)),
            );

        div()
            .flex()
            .size_full()
            .bg(rgb(0x111418))
            .text_color(rgb(0xe6edf3))
            .child(
                div()
                    .w(px(280.0))
                    .h_full()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .bg(rgb(0x161b22))
                    .child(div().text_sm().child("Playlist"))
                    .child(div().flex_1().child(playlist)),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .flex_col()
                    .overflow_hidden()
                    .p_4()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .child(div().text_xl().child(self.current_title.clone())),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .overflow_hidden()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(0x30363d))
                            .p_2()
                            .child(video_area)
                            .child(viewport_observer)
                            .child(overlay_progress_bar),
                    )
                    .child(
                        div()
                            .flex_none()
                            .px_3()
                            .py_2()
                            .rounded_sm()
                            .bg(rgb(0x1f242d))
                            .text_color(rgb(0x9fb3c8))
                            .child(message_text),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_3()
                            .px_3()
                            .py_2()
                            .rounded_sm()
                            .bg(rgb(0x161b22))
                            .child(previous_button)
                            .child(playback_toggle)
                            .child(next_button),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Direction, ShutdownOrder, adjacent_index, bottom_control_labels, can_toggle_playback,
        handle_backend_failure, handle_file_title_update, handle_render_failure,
        normalize_video_viewport, overlay_progress_bottom_inset_px, overlay_progress_fill_fraction,
        overlay_progress_horizontal_inset_px, reset_progress, selection_changed,
        should_render_frame, viewport_changed,
    };
    use crate::state::AppState;
    use gpui::{Bounds, point, px, size};
    use std::{cell::RefCell, rc::Rc};

    struct DropRecorder {
        name: &'static str,
        drops: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Drop for DropRecorder {
        fn drop(&mut self) {
            self.drops.borrow_mut().push(self.name);
        }
    }

    #[test]
    fn selection_changed_is_false_for_same_selection() {
        assert!(!selection_changed(Some(2), Some(2)));
    }

    #[test]
    fn selection_changed_is_true_for_new_selection() {
        assert!(selection_changed(Some(1), Some(2)));
    }

    #[test]
    fn reset_progress_clears_stale_playback_values() {
        let mut state = AppState::from_playlist(Vec::new());
        state.update_progress(12.5, 98.0);

        reset_progress(&mut state);

        assert_eq!(state.playback_position_seconds, 0.0);
        assert_eq!(state.playback_duration_seconds, 0.0);
    }

    #[test]
    fn handle_backend_failure_sets_error_and_resets_progress() {
        let mut state = AppState::from_playlist(Vec::new());
        state.update_progress(12.5, 98.0);

        handle_backend_failure(&mut state, "fatal backend error");

        assert_eq!(state.playback_position_seconds, 0.0);
        assert_eq!(state.playback_duration_seconds, 0.0);
        assert_eq!(state.error_message.as_deref(), Some("fatal backend error"));
    }

    #[test]
    fn handle_render_failure_sets_status_and_resets_progress() {
        let mut state = AppState::from_playlist(Vec::new());
        let mut status_message = String::new();
        state.update_progress(12.5, 98.0);

        handle_render_failure(&mut state, &mut status_message, "transient render failure");

        assert_eq!(state.playback_position_seconds, 0.0);
        assert_eq!(state.playback_duration_seconds, 0.0);
        assert_eq!(status_message, "transient render failure");
        assert_eq!(state.error_message, None);
    }

    #[test]
    fn normalize_video_viewport_rejects_zero_sized_bounds() {
        let zero_width = Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(180.0)));
        let zero_height = Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(0.0)));

        assert_eq!(normalize_video_viewport(zero_width), None);
        assert_eq!(normalize_video_viewport(zero_height), None);
    }

    #[test]
    fn normalize_video_viewport_floors_fractional_pixel_sizes() {
        let bounds = Bounds::new(point(px(10.0), px(12.0)), size(px(640.8), px(359.9)));

        assert_eq!(normalize_video_viewport(bounds), Some((640, 359)));
    }

    #[test]
    fn viewport_changed_only_reports_real_differences() {
        let first = Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(240.0)));
        let second = Bounds::new(point(px(0.0), px(0.0)), size(px(400.0), px(240.0)));

        assert!(viewport_changed(None, first));
        assert!(!viewport_changed(Some(first), first));
        assert!(viewport_changed(Some(first), second));
    }

    #[test]
    fn should_render_frame_requires_a_valid_viewport() {
        assert!(should_render_frame(true, true, false, true));
        assert!(!should_render_frame(true, true, false, false));
        assert!(!should_render_frame(true, true, true, true));
        assert!(!should_render_frame(false, true, false, true));
        assert!(!should_render_frame(true, false, false, true));
    }

    #[test]
    fn can_toggle_playback_disables_controls_while_error_is_present() {
        assert!(can_toggle_playback(true, true, false));
        assert!(!can_toggle_playback(true, true, true));
        assert!(!can_toggle_playback(false, true, false));
        assert!(!can_toggle_playback(true, false, false));
    }

    #[test]
    fn overlay_progress_fill_fraction_stays_in_unit_interval() {
        assert_eq!(overlay_progress_fill_fraction(0.25), 0.25);
        assert_eq!(overlay_progress_fill_fraction(2.0), 1.0);
        assert_eq!(overlay_progress_fill_fraction(-1.0), 0.0);
    }

    #[test]
    fn overlay_progress_bottom_inset_matches_overlay_contract() {
        assert_eq!(overlay_progress_bottom_inset_px(), 12.0);
    }

    #[test]
    fn overlay_progress_horizontal_inset_matches_overlay_contract() {
        assert_eq!(overlay_progress_horizontal_inset_px(), 14.0);
    }

    #[test]
    fn bottom_control_strip_keeps_only_transport_buttons() {
        let labels = bottom_control_labels();

        assert_eq!(labels, ["Previous", "Play/Pause", "Next"]);
    }

    #[test]
    fn handle_file_title_update_clears_stale_status_message_on_recovery() {
        let mut state = AppState::from_playlist(Vec::new());
        let mut current_title = String::from("Old title");
        let mut status_message = String::from("Failed to preload clip.mp4");
        state.set_error("backend failed");

        handle_file_title_update(
            &mut current_title,
            &mut state,
            &mut status_message,
            "Recovered title".to_string(),
        );

        assert_eq!(current_title, "Recovered title");
        assert_eq!(state.error_message, None);
        assert!(status_message.is_empty());
    }

    #[test]
    fn adjacent_index_stays_in_bounds_at_playlist_edges() {
        assert_eq!(adjacent_index(Some(0), 3, Direction::Previous), None);
        assert_eq!(adjacent_index(Some(2), 3, Direction::Next), None);
    }

    #[test]
    fn adjacent_index_moves_to_previous_and_next_items() {
        assert_eq!(adjacent_index(Some(1), 3, Direction::Previous), Some(0));
        assert_eq!(adjacent_index(Some(1), 3, Direction::Next), Some(2));
    }

    #[test]
    fn shutdown_order_drops_dependent_before_owner() {
        let drops = Rc::new(RefCell::new(Vec::new()));
        let recorded_drops = Rc::clone(&drops);

        let presenter = DropRecorder {
            name: "presenter",
            drops: Rc::clone(&drops),
        };
        let backend = DropRecorder {
            name: "backend",
            drops,
        };

        drop(ShutdownOrder::new(Some(backend), Some(presenter)));

        assert_eq!(&*recorded_drops.borrow(), &["presenter", "backend"]);
    }
}
