use std::path::PathBuf;

use tiny::{media::PlaylistEntry, state::AppState};

fn entry(name: &str) -> PlaylistEntry {
    PlaylistEntry {
        path: PathBuf::from(name),
        display_name: name.to_owned(),
    }
}

#[test]
fn from_playlist_selects_first_item_and_starts_paused() {
    let first = entry("alpha.mp4");
    let second = entry("beta.webm");
    let state = AppState::from_playlist(vec![first.clone(), second]);

    assert_eq!(state.selected_index, Some(0));
    assert_eq!(state.current_entry(), Some(&first));
    assert_eq!(state.current_title(), "alpha.mp4");
    assert!(!state.is_playing);
    assert!(state.error_message.is_none());
    assert!(state.can_control_playback());
}

#[test]
fn select_resets_playback_and_clears_error() {
    let first = entry("alpha.mp4");
    let second = entry("beta.webm");
    let mut state = AppState::from_playlist(vec![first, second.clone()]);

    state.sync_pause_state(false);
    state.set_error("failed to play");
    state.select(1);

    assert_eq!(state.selected_index, Some(1));
    assert_eq!(state.current_entry(), Some(&second));
    assert_eq!(state.current_title(), "beta.webm");
    assert!(!state.is_playing);
    assert!(state.error_message.is_none());
}

#[test]
fn set_error_stops_playback() {
    let mut state = AppState::from_playlist(vec![entry("alpha.mp4")]);

    state.sync_pause_state(false);
    state.set_error("failed to play");

    assert!(!state.is_playing);
    assert_eq!(state.error_message.as_deref(), Some("failed to play"));
}

#[test]
fn reselecting_current_item_is_a_no_op() {
    let mut state = AppState::from_playlist(vec![entry("alpha.mp4"), entry("beta.webm")]);

    state.sync_pause_state(false);
    state.set_error("transient warning");
    state.select(0);

    assert_eq!(state.selected_index, Some(0));
    assert!(!state.is_playing);
    assert_eq!(state.error_message.as_deref(), Some("transient warning"));
}

#[test]
fn from_playlist_handles_empty_playlists() {
    let state = AppState::from_playlist(Vec::new());

    assert_eq!(state.selected_index, None);
    assert_eq!(state.current_entry(), None);
    assert_eq!(state.current_title(), "No video selected");
    assert!(!state.is_playing);
    assert!(state.error_message.is_none());
    assert!(!state.can_control_playback());
}

#[test]
fn navigation_availability_tracks_selected_index() {
    let mut state = AppState::from_playlist(vec![
        entry("alpha.mp4"),
        entry("beta.webm"),
        entry("gamma.mkv"),
    ]);

    assert!(!state.has_previous());
    assert!(state.has_next());

    state.select(1);

    assert!(state.has_previous());
    assert!(state.has_next());

    state.select(2);

    assert!(state.has_previous());
    assert!(!state.has_next());
}

#[test]
fn selecting_a_new_item_resets_progress() {
    let mut state = AppState::from_playlist(vec![entry("alpha.mp4"), entry("beta.webm")]);

    state.update_progress(12.5, 50.0);
    state.select(1);

    assert_eq!(state.playback_position_seconds, 0.0);
    assert_eq!(state.playback_duration_seconds, 0.0);
    assert_eq!(state.progress_fraction(), 0.0);
}

#[test]
fn progress_fraction_uses_position_and_duration() {
    let mut state = AppState::from_playlist(vec![entry("alpha.mp4")]);

    state.update_progress(25.0, 100.0);

    assert_eq!(state.progress_fraction(), 0.25);
}

#[test]
fn has_next_returns_false_for_invalid_selected_index() {
    let mut state = AppState::from_playlist(vec![entry("alpha.mp4")]);

    state.selected_index = Some(usize::MAX);

    assert!(!state.has_next());
}
