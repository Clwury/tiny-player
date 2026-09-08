//! Local single-line editor surface.
//!
//! This is a dependency-light adaptation of Zed's `Editor::single_line`.
//! Zed's complete editor crate also pulls in its workspace, language, project,
//! and multi-buffer crates, so copying that crate wholesale would add a large
//! unrelated dependency graph to tiny-player. The implementation below keeps
//! the single-line editing state, GPUI input handling, and IME behavior local.
//!
//! The constructor and movement semantics follow
//! `zed/crates/editor/src/editor.rs::Editor::single_line`; the caret follows
//! Zed's `CursorLayout` bar proportions and blink cadence.

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use gpui::{
    App, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    GlobalElementId, InteractiveElement, IntoElement, KeyBinding, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, ParentElement, Pixels, Point, Render,
    ShapedLine, SharedString, StatefulInteractiveElement, Style, Styled, Task, TextRun,
    UTF16Selection, UnderlineStyle, Window, actions, div, fill, point, prelude::FluentBuilder, px,
    relative, size, svg,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme;

/// Keep the caret proportions in sync with Zed's default `CursorShape::Bar`.
///
/// Zed uses a two-pixel bar that spans the complete editor line. The value is
/// expressed in logical pixels here and snapped to the display scale before it
/// is painted (see [`snap_bounds_to_device_pixels`]).
const CURSOR_WIDTH: Pixels = px(2.0);
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);

actions!(
    editor,
    [
        Backspace,
        Delete,
        DeleteToBeginning,
        DeleteToEnd,
        DeletePreviousWord,
        DeleteNextWord,
        Left,
        Right,
        PreviousWord,
        NextWord,
        SelectLeft,
        SelectRight,
        SelectPreviousWord,
        SelectNextWord,
        SelectToStart,
        SelectToEnd,
        SelectAll,
        Home,
        End,
        MoveToStart,
        MoveToEnd,
        Undo,
        Redo,
        Escape,
        Paste,
        Cut,
        Copy,
        Submit,
    ]
);

fn utf8_offset_from_utf16(text: &str, target: usize) -> usize {
    let mut utf8_offset = 0;
    let mut utf16_count = 0;

    for ch in text.chars() {
        if utf16_count >= target {
            break;
        }
        utf16_count += ch.len_utf16();
        utf8_offset += ch.len_utf8();
    }

    utf8_offset
}

fn utf16_offset_from_utf8(text: &str, target: usize) -> usize {
    let mut utf16_offset = 0;
    let mut utf8_count = 0;

    for ch in text.chars() {
        if utf8_count >= target {
            break;
        }
        utf8_count += ch.len_utf8();
        utf16_offset += ch.len_utf16();
    }

    utf16_offset
}

fn range_from_utf16_in_text(text: &str, range_utf16: &Range<usize>) -> Range<usize> {
    utf8_offset_from_utf16(text, range_utf16.start)..utf8_offset_from_utf16(text, range_utf16.end)
}

fn range_to_utf16_in_text(text: &str, range: &Range<usize>) -> Range<usize> {
    utf16_offset_from_utf8(text, range.start)..utf16_offset_from_utf8(text, range.end)
}

fn selected_range_for_marked_text(
    inserted_at: usize,
    inserted_text: &str,
    selected_range_utf16: Option<&Range<usize>>,
) -> Range<usize> {
    selected_range_utf16
        .map(|range| range_from_utf16_in_text(inserted_text, range))
        .map(|range| {
            if range.start <= range.end {
                range
            } else {
                range.end..range.start
            }
        })
        .map(|range| inserted_at + range.start..inserted_at + range.end)
        .unwrap_or_else(|| {
            let cursor = inserted_at + inserted_text.len();
            cursor..cursor
        })
}

fn filter_editor_text(text: &str, digits_only: bool, max_chars: Option<usize>) -> String {
    let mut filtered = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        let character = if character == '\r' {
            if characters.peek() == Some(&'\n') {
                characters.next();
            }
            ' '
        } else if character == '\n' {
            ' '
        } else {
            character
        };
        if !digits_only || character.is_ascii_digit() {
            filtered.push(character);
        }
    }

    max_chars
        .map(|max_chars| filtered.graphemes(true).take(max_chars).collect())
        .unwrap_or(filtered)
}

fn grapheme_count(text: &str) -> usize {
    text.graphemes(true).count()
}

/// Snap a logical-pixel rectangle to physical pixel boundaries.
///
/// Text shaping can place a caret on a fractional logical coordinate (most
/// noticeably on a fractional display scale).  Painting that rectangle
/// directly makes a one/two-pixel caret look soft or vary in width as it moves
/// through the field.  Zed's editor snaps its `CursorLayout` bounds before
/// painting; GPUI 0.2 exposes the equivalent conversion helpers, so keep the
/// same behaviour locally.
fn snap_bounds_to_device_pixels(bounds: Bounds<Pixels>, scale_factor: f32) -> Bounds<Pixels> {
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return bounds;
    }

    bounds
        .to_device_pixels(scale_factor)
        .to_pixels(scale_factor)
}

/// Build the bar cursor bounds while keeping the caret inside the text
/// viewport.  The clamp matters when the last glyph is wider than the field or
/// when a resized field is temporarily narrower than the caret itself.
fn cursor_bounds(bounds: Bounds<Pixels>, cursor_x: Pixels, scale_factor: f32) -> Bounds<Pixels> {
    let viewport_width = bounds.size.width.max(Pixels::ZERO);
    let cursor_width = CURSOR_WIDTH.min(viewport_width);
    let max_cursor_x = (viewport_width - cursor_width).max(Pixels::ZERO);
    let cursor_x = cursor_x.max(Pixels::ZERO).min(max_cursor_x);
    let height = bounds.size.height.max(Pixels::ZERO);

    snap_bounds_to_device_pixels(
        Bounds::new(
            point(bounds.left() + cursor_x, bounds.top()),
            size(cursor_width, height),
        ),
        scale_factor,
    )
}

/// Return whether the caret is visible at a point in its blink cycle.
///
/// Zed starts a focused caret visible, then toggles it every 500ms. Keeping the
/// calculation pure makes the edge behaviour explicit and easy to regression
/// test without constructing a GPUI window.
fn cursor_visible_after(elapsed: Duration) -> bool {
    let interval_ms = CURSOR_BLINK_INTERVAL.as_millis();
    interval_ms == 0 || (elapsed.as_millis() / interval_ms).is_multiple_of(2)
}

fn is_word_grapheme(grapheme: &str) -> bool {
    grapheme
        .chars()
        .any(|character| character.is_alphanumeric() || character == '_')
}

fn clamp_utf8_boundary(text: &str, offset: usize) -> usize {
    let offset = offset.min(text.len());
    if offset == 0 || offset == text.len() {
        return offset;
    }

    let previous = text
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .take_while(|index| *index <= offset)
        .last()
        .unwrap_or(0);
    let next = text
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .find(|index| *index >= offset)
        .unwrap_or(text.len());
    if offset - previous < next.saturating_sub(offset) {
        previous
    } else {
        next
    }
}

fn previous_word_boundary_in_text(text: &str, offset: usize) -> usize {
    let offset = clamp_utf8_boundary(text, offset);
    let prefix = &text[..offset];
    let mut boundary = 0;
    let mut seen_word = false;

    // Skip the current whitespace/punctuation run, then stop at the start of
    // the preceding word. This is the same shape as Zed's
    // `previous_word_start` movement for a single line.
    for (index, grapheme) in prefix.grapheme_indices(true).rev() {
        if is_word_grapheme(grapheme) {
            seen_word = true;
            boundary = index;
        } else if seen_word {
            break;
        } else {
            boundary = index;
        }
    }
    boundary
}

fn next_word_boundary_in_text(text: &str, offset: usize) -> usize {
    let offset = clamp_utf8_boundary(text, offset);
    let suffix = &text[offset..];
    let mut seen_word = false;
    let mut boundary = text.len();

    // Move to the end of the current/next word, skipping separators before
    // it. This mirrors Zed's `next_word_end` behavior.
    for (relative, grapheme) in suffix.grapheme_indices(true) {
        let index = offset + relative;
        if is_word_grapheme(grapheme) {
            seen_word = true;
            boundary = index + grapheme.len();
        } else if seen_word {
            boundary = index;
            break;
        } else {
            boundary = index + grapheme.len();
        }
    }
    boundary
}

fn word_range_at(text: &str, offset: usize) -> Range<usize> {
    let graphemes = text.grapheme_indices(true).collect::<Vec<_>>();
    if graphemes.is_empty() {
        return 0..0;
    }

    let offset = clamp_utf8_boundary(text, offset);
    let target = graphemes
        .iter()
        .position(|(index, _)| *index > offset)
        .map(|index| index.saturating_sub(1))
        .unwrap_or(graphemes.len() - 1);
    let word = is_word_grapheme(graphemes[target].1);

    let mut start = target;
    while start > 0 && is_word_grapheme(graphemes[start - 1].1) == word {
        start -= 1;
    }
    let end = graphemes
        .get(target + 1)
        .map(|(index, _)| *index)
        .unwrap_or(text.len());
    let mut end_index = target + 1;
    while end_index < graphemes.len() && is_word_grapheme(graphemes[end_index].1) == word {
        end_index += 1;
    }

    graphemes[start].0..if end_index < graphemes.len() {
        graphemes[end_index].0
    } else {
        end
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorEvent {
    Changed,
    Submitted,
}

/// A focused, single-line editor modeled after Zed's `Editor::single_line`.
///
/// Zed's full editor crate is coupled to its project, language-server, and
/// multi-buffer crates. This self-contained core keeps the same single-line
/// editing model while retaining tiny-player's GPUI/theme integration.
pub struct Editor {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    scroll_offset: Pixels,
    /// Monotonic timestamp of the most recent caret movement/edit.  This is
    /// used instead of wall-clock time so clock adjustments cannot make the
    /// caret flicker or remain hidden.
    last_cursor_activity: Instant,
    /// Whether the editor was focused during the previous paint.  A focus
    /// transition always reveals the caret immediately, matching Zed's
    /// `BlinkManager::enable` behaviour.
    was_focused: bool,
    /// Whether a blink timer is currently allowed to wake this editor.
    cursor_blink_enabled: bool,
    /// The next instant at which the caret should toggle visibility.
    cursor_blink_deadline: Instant,
    /// Avoid spawning a second blink task while the current timer is waiting.
    cursor_blink_task_active: bool,
    cursor_blink_task: Task<()>,
    is_selecting: bool,
    masked: bool,
    digits_only: bool,
    max_chars: Option<usize>,
    borderless: bool,
    compact: bool,
    centered: bool,
    input_height: Pixels,
    clearable: bool,
    /// Render a password visibility control in the editor's suffix slot.
    mask_toggle: bool,
    history: Vec<EditSnapshot>,
    history_index: usize,
    restoring_history: bool,
    composition_snapshot: Option<EditSnapshot>,
}

#[derive(Clone)]
struct EditSnapshot {
    content: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
}

impl Editor {
    pub fn bind_keys(cx: &mut App) {
        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, Some("Editor")),
            KeyBinding::new("delete", Delete, Some("Editor")),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-backspace", DeleteToBeginning, Some("Editor")),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-delete", DeleteToEnd, Some("Editor")),
            #[cfg(target_os = "macos")]
            KeyBinding::new("alt-backspace", DeletePreviousWord, Some("Editor")),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-backspace", DeletePreviousWord, Some("Editor")),
            #[cfg(target_os = "macos")]
            KeyBinding::new("alt-delete", DeleteNextWord, Some("Editor")),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-delete", DeleteNextWord, Some("Editor")),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-u", DeleteToBeginning, Some("Editor")),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-k", DeleteToEnd, Some("Editor")),
            KeyBinding::new("left", Left, Some("Editor")),
            KeyBinding::new("right", Right, Some("Editor")),
            #[cfg(target_os = "macos")]
            KeyBinding::new("alt-left", PreviousWord, Some("Editor")),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-left", PreviousWord, Some("Editor")),
            #[cfg(target_os = "macos")]
            KeyBinding::new("alt-right", NextWord, Some("Editor")),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-right", NextWord, Some("Editor")),
            KeyBinding::new("shift-left", SelectLeft, Some("Editor")),
            KeyBinding::new("shift-right", SelectRight, Some("Editor")),
            KeyBinding::new("shift-home", SelectToStart, Some("Editor")),
            KeyBinding::new("shift-end", SelectToEnd, Some("Editor")),
            #[cfg(target_os = "macos")]
            KeyBinding::new("alt-shift-left", SelectPreviousWord, Some("Editor")),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-shift-left", SelectPreviousWord, Some("Editor")),
            #[cfg(target_os = "macos")]
            KeyBinding::new("alt-shift-right", SelectNextWord, Some("Editor")),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-shift-right", SelectNextWord, Some("Editor")),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-shift-left", SelectToStart, Some("Editor")),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-shift-right", SelectToEnd, Some("Editor")),
            KeyBinding::new("cmd-a", SelectAll, Some("Editor")),
            KeyBinding::new("ctrl-a", SelectAll, Some("Editor")),
            KeyBinding::new("cmd-v", Paste, Some("Editor")),
            KeyBinding::new("ctrl-v", Paste, Some("Editor")),
            KeyBinding::new("cmd-c", Copy, Some("Editor")),
            KeyBinding::new("ctrl-c", Copy, Some("Editor")),
            KeyBinding::new("cmd-x", Cut, Some("Editor")),
            KeyBinding::new("ctrl-x", Cut, Some("Editor")),
            KeyBinding::new("home", Home, Some("Editor")),
            KeyBinding::new("end", End, Some("Editor")),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-e", MoveToEnd, Some("Editor")),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-left", MoveToStart, Some("Editor")),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-right", MoveToEnd, Some("Editor")),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-z", Undo, Some("Editor")),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-z", Undo, Some("Editor")),
            #[cfg(target_os = "macos")]
            KeyBinding::new("cmd-shift-z", Redo, Some("Editor")),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-y", Redo, Some("Editor")),
            #[cfg(not(target_os = "macos"))]
            KeyBinding::new("ctrl-shift-z", Redo, Some("Editor")),
            KeyBinding::new("escape", Escape, Some("Editor")),
            KeyBinding::new("enter", Submit, Some("Editor")),
        ]);
    }

    /// Construct an editor in Zed's single-line mode.
    ///
    /// The `Window` parameter intentionally mirrors Zed's API. The compact
    /// editor does not need to build its text layout until it is rendered, so
    /// the state-only constructors use [`Self::single_line_without_window`].
    pub fn single_line(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::single_line_without_window(cx)
    }

    /// Construct the same editor while creating an entity outside a window
    /// callback. GPUI's `AppContext::new` only supplies an entity context, so
    /// this is the path used by the app's dialogs and page state.
    fn single_line_without_window(cx: &mut Context<Self>) -> Self {
        let mut editor = Self {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: "".into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            scroll_offset: px(0.0),
            last_cursor_activity: Instant::now(),
            was_focused: false,
            cursor_blink_enabled: false,
            cursor_blink_deadline: Instant::now() + CURSOR_BLINK_INTERVAL,
            cursor_blink_task_active: false,
            cursor_blink_task: Task::ready(()),
            is_selecting: false,
            masked: false,
            digits_only: false,
            max_chars: None,
            borderless: false,
            compact: false,
            centered: false,
            input_height: px(34.0),
            clearable: false,
            mask_toggle: false,
            history: Vec::new(),
            history_index: 0,
            restoring_history: false,
            composition_snapshot: None,
        };
        editor.push_history_snapshot();
        editor
    }

    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        let mut editor = Self::single_line_without_window(cx);
        let placeholder = placeholder.into();
        editor.placeholder = filter_editor_text(&placeholder, false, None).into();
        editor
    }

    /// Window-aware convenience constructor matching the full Zed editor's
    /// construction flow.
    #[allow(dead_code)]
    pub fn new_with_window(
        placeholder: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut editor = Self::single_line(window, cx);
        let placeholder = placeholder.into();
        editor.placeholder = filter_editor_text(&placeholder, false, None).into();
        editor
    }

    pub fn default_value(mut self, value: impl Into<SharedString>) -> Self {
        self.content = self.filtered(value.into().as_ref()).into();
        self.selected_range = self.content.len()..self.content.len();
        self.note_cursor_activity();
        self.history.clear();
        self.history_index = 0;
        self.push_history_snapshot();
        self
    }

    pub fn masked(mut self, masked: bool) -> Self {
        self.masked = masked;
        self
    }

    pub fn digits_only(mut self) -> Self {
        self.digits_only = true;
        self.content = self.filtered(self.content.as_ref()).into();
        self.selected_range = self.content.len()..self.content.len();
        self.note_cursor_activity();
        self.reset_history();
        self
    }

    pub fn max_chars(mut self, max_chars: usize) -> Self {
        self.max_chars = Some(max_chars);
        self.content = self.filtered(self.content.as_ref()).into();
        self.selected_range = self.content.len()..self.content.len();
        self.note_cursor_activity();
        self.reset_history();
        self
    }

    pub fn borderless(mut self) -> Self {
        self.borderless = true;
        self
    }

    /// Match Zed's compact settings controls without changing other editors.
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self.input_height = px(28.0);
        self
    }

    pub fn height(mut self, height: Pixels) -> Self {
        self.input_height = height;
        self
    }

    pub fn centered(mut self) -> Self {
        self.centered = true;
        self
    }

    pub fn clearable(mut self) -> Self {
        self.clearable = true;
        self
    }

    /// Show an eye icon in the editor suffix that toggles password masking.
    pub fn mask_toggle(mut self) -> Self {
        self.mask_toggle = true;
        self
    }

    pub fn value(&self) -> SharedString {
        self.content.clone()
    }

    /// This compact editor intentionally exposes only Zed's single-line mode.
    #[allow(dead_code)]
    pub fn is_single_line(&self) -> bool {
        true
    }

    /// Whether the editor buffer contains no text.
    #[allow(dead_code)]
    pub fn is_empty(&self, _: &App) -> bool {
        self.content.is_empty()
    }

    /// Return the current buffer text, matching Zed's editor API.
    #[allow(dead_code)]
    pub fn text(&self, _: &App) -> String {
        self.content.to_string()
    }

    /// Return the trimmed buffer, or `None` when it contains only whitespace.
    #[allow(dead_code)]
    pub fn text_option(&self, _: &App) -> Option<String> {
        let text = self.content.trim();
        (!text.is_empty()).then(|| text.to_string())
    }

    /// Return the placeholder when one has been configured.
    #[allow(dead_code)]
    pub fn placeholder_text(&self, _: &mut App) -> Option<String> {
        (!self.placeholder.is_empty()).then(|| self.placeholder.to_string())
    }

    /// Replace the complete buffer text and move the cursor to its end.
    #[allow(dead_code)]
    pub fn set_text(
        &mut self,
        value: impl Into<SharedString>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_value(value, cx);
    }

    /// Set the placeholder text after construction.
    #[allow(dead_code)]
    pub fn set_placeholder_text(
        &mut self,
        placeholder: impl Into<SharedString>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let placeholder = placeholder.into();
        self.placeholder = filter_editor_text(&placeholder, false, None).into();
        cx.notify();
    }

    /// Move the insertion point to the end of the buffer.
    #[allow(dead_code)]
    pub fn move_selection_to_end(&mut self, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    /// Select the complete buffer.
    #[allow(dead_code)]
    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        self.note_cursor_activity();
        cx.notify();
    }

    pub fn set_value(&mut self, value: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = self.filtered(value.into().as_ref()).into();
        self.selected_range = self.content.len()..self.content.len();
        self.note_cursor_activity();
        self.marked_range = None;
        self.composition_snapshot = None;
        self.reset_history();
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    pub fn set_masked(&mut self, masked: bool, cx: &mut Context<Self>) {
        self.masked = masked;
        self.note_cursor_activity();
        cx.notify();
    }

    fn clear_button(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.clear_value(cx);
    }

    fn mask_toggle_button(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.set_masked(!self.masked, cx);
        self.focus_handle.focus(window, cx);
    }

    /// Clear the editor's buffer, matching Zed's public editor operation.
    #[allow(dead_code)]
    pub fn clear(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.clear_value(cx);
    }

    #[allow(dead_code)]
    pub fn clear_value(&mut self, cx: &mut Context<Self>) {
        self.content = "".into();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.note_cursor_activity();
        self.marked_range = None;
        self.composition_snapshot = None;
        self.reset_history();
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    pub fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn filtered(&self, text: &str) -> String {
        filter_editor_text(text, self.digits_only, self.max_chars)
    }

    fn replacement_for_range(&self, range: &Range<usize>, new_text: &str) -> String {
        let new_text = self.filtered(new_text);
        let Some(max_chars) = self.max_chars else {
            return new_text;
        };

        let existing_chars = grapheme_count(&self.content[..range.start])
            + grapheme_count(&self.content[range.end..]);
        let available_chars = max_chars.saturating_sub(existing_chars);
        new_text.graphemes(true).take(available_chars).collect()
    }

    fn snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            content: self.content.clone(),
            selected_range: self.selected_range.clone(),
            selection_reversed: self.selection_reversed,
        }
    }

    fn push_history_snapshot(&mut self) {
        if self.restoring_history {
            return;
        }

        let snapshot = self.snapshot();
        if self
            .history
            .get(self.history_index)
            .is_some_and(|previous| {
                previous.content == snapshot.content
                    && previous.selected_range == snapshot.selected_range
                    && previous.selection_reversed == snapshot.selection_reversed
            })
        {
            return;
        }

        self.history.truncate(self.history_index.saturating_add(1));
        self.history.push(snapshot);
        self.history_index = self.history.len().saturating_sub(1);

        // Input fields are short-lived, but keeping a bounded history avoids
        // retaining pasted text forever when a dialog is left open.
        const MAX_HISTORY: usize = 128;
        if self.history.len() > MAX_HISTORY {
            let excess = self.history.len() - MAX_HISTORY;
            self.history.drain(..excess);
            self.history_index = self.history_index.saturating_sub(excess);
        }
    }

    fn reset_history(&mut self) {
        self.history.clear();
        self.history_index = 0;
        self.push_history_snapshot();
    }

    fn restore_history_snapshot(&mut self, snapshot: EditSnapshot, cx: &mut Context<Self>) {
        self.restoring_history = true;
        self.content = snapshot.content;
        self.selected_range = snapshot.selected_range;
        self.selection_reversed = snapshot.selection_reversed;
        self.note_cursor_activity();
        self.marked_range = None;
        self.composition_snapshot = None;
        self.scroll_offset = px(0.0);
        self.restoring_history = false;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if self.history_index == 0 {
            return;
        }
        self.history_index -= 1;
        let snapshot = self.history[self.history_index].clone();
        self.restore_history_snapshot(snapshot, cx);
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        let Some(next_index) = self.history_index.checked_add(1) else {
            return;
        };
        let Some(snapshot) = self.history.get(next_index).cloned() else {
            return;
        };
        self.history_index = next_index;
        self.restore_history_snapshot(snapshot, cx);
    }

    fn previous_word_boundary(&self, offset: usize) -> usize {
        previous_word_boundary_in_text(&self.content, offset)
    }

    fn next_word_boundary(&self, offset: usize) -> usize {
        next_word_boundary_in_text(&self.content, offset)
    }

    fn previous_word(&mut self, _: &PreviousWord, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.previous_word_boundary(self.cursor_offset());
        if self.selected_range.is_empty() {
            self.move_to(target, cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn next_word(&mut self, _: &NextWord, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.next_word_boundary(self.cursor_offset());
        if self.selected_range.is_empty() {
            self.move_to(target, cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn select_previous_word(
        &mut self,
        _: &SelectPreviousWord,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.previous_word_boundary(self.cursor_offset());
        self.select_to(target, cx);
    }

    fn select_next_word(&mut self, _: &SelectNextWord, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.next_word_boundary(self.cursor_offset());
        self.select_to(target, cx);
    }

    fn select_word_at(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = word_range_at(&self.content, offset);
        self.selection_reversed = false;
        self.note_cursor_activity();
        cx.notify();
    }

    fn select_to_start(&mut self, _: &SelectToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_to_end(&mut self, _: &SelectToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    fn delete_to_beginning(
        &mut self,
        _: &DeleteToBeginning,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            self.select_to(0, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete_to_end(&mut self, _: &DeleteToEnd, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.content.len(), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete_previous_word(
        &mut self,
        _: &DeletePreviousWord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_word_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete_next_word(
        &mut self,
        _: &DeleteNextWord,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_word_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn move_to_start(&mut self, _: &MoveToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn move_to_end(&mut self, _: &MoveToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(snapshot) = self.composition_snapshot.take() {
            self.content = snapshot.content;
            self.selected_range = snapshot.selected_range;
            self.selection_reversed = snapshot.selection_reversed;
            self.note_cursor_activity();
            self.marked_range = None;
            self.scroll_offset = px(0.0);
            cx.emit(EditorEvent::Changed);
            cx.notify();
        } else if self.marked_range.take().is_some() {
            cx.notify();
        }
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all_text(cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(EditorEvent::Submitted);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        let offset = self.index_for_mouse_position(event.position);
        match event.click_count {
            3.. => {
                self.select_all_text(cx);
                self.is_selecting = false;
            }
            2 => {
                self.select_word_at(offset, cx);
                self.is_selecting = true;
            }
            _ => {
                self.is_selecting = true;
                if event.modifiers.shift {
                    self.select_to(offset, cx);
                } else {
                    self.move_to(offset, cx)
                }
            }
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.clamp_to_boundary(offset);
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        self.note_cursor_activity();
        cx.notify()
    }

    fn update_scroll_offset(&mut self, line: &ShapedLine, width: Pixels) {
        self.scroll_offset = Self::scroll_offset_for_cursor(
            line,
            width,
            self.display_offset_for_content_offset(self.cursor_offset()),
            self.scroll_offset,
        );
    }

    fn scroll_offset_for_cursor(
        line: &ShapedLine,
        width: Pixels,
        cursor_offset: usize,
        current_scroll: Pixels,
    ) -> Pixels {
        let cursor_x = line.x_for_index(cursor_offset.min(line.text.len()));
        let padding = px(8.0);
        let cursor_width = CURSOR_WIDTH;
        let right_edge = width - padding - cursor_width;
        let max_scroll = (line.width - width + padding + cursor_width).max(px(0.0));

        let scroll_offset = if cursor_x - current_scroll > right_edge {
            cursor_x - right_edge
        } else if cursor_x < current_scroll + padding {
            (cursor_x - padding).max(px(0.0))
        } else {
            current_scroll
        };

        scroll_offset.min(max_scroll).max(px(0.0))
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn note_cursor_activity(&mut self) {
        let now = Instant::now();
        self.last_cursor_activity = now;
        self.cursor_blink_deadline = now + CURSOR_BLINK_INTERVAL;
    }

    /// Keep the caret blink on a timer instead of requesting a full animation
    /// frame continuously. This matters on pages that keep the editor focused
    /// (notably Search): an always-running animation frame would redraw every
    /// card on every display tick, including while the window is being resized.
    fn sync_cursor_blink(&mut self, focused: bool, cx: &mut Context<Self>) {
        if self.was_focused != focused {
            self.was_focused = focused;
            if focused {
                self.note_cursor_activity();
            }
        }

        self.cursor_blink_enabled = focused;
        if !focused {
            self.cursor_blink_task_active = false;
            self.cursor_blink_task = Task::ready(());
            return;
        }
        if self.cursor_blink_task_active {
            return;
        }

        self.cursor_blink_task_active = true;
        self.cursor_blink_task = cx.spawn(async move |editor, cx| {
            while let Some(delay) = editor
                .update(cx, |editor, _| {
                    if !editor.cursor_blink_enabled {
                        return None;
                    }
                    Some(
                        editor
                            .cursor_blink_deadline
                            .saturating_duration_since(Instant::now()),
                    )
                })
                .ok()
                .flatten()
            {
                if !delay.is_zero() {
                    cx.background_executor().timer(delay).await;
                    continue;
                }

                editor
                    .update(cx, |editor, cx| {
                        if editor.cursor_blink_enabled {
                            editor.cursor_blink_deadline = Instant::now() + CURSOR_BLINK_INTERVAL;
                            editor.cursor_blink_task_active = false;
                            cx.notify();
                        }
                    })
                    .ok();
                break;
            }
        });
    }

    fn cursor_is_visible(&self, focused: bool, now: Instant) -> bool {
        if !focused {
            return false;
        }

        // Reveal the caret on the first frame after focus.  Subsequent frames
        // follow the same 500ms on/off cadence as Zed's BlinkManager.
        !self.was_focused
            || cursor_visible_after(now.saturating_duration_since(self.last_cursor_activity))
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }

        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        let display_index =
            line.closest_index_for_x(position.x - bounds.left() + self.scroll_offset);
        self.content_offset_for_display_offset(display_index)
    }

    fn display_offset_for_content_offset(&self, offset: usize) -> usize {
        if !self.masked {
            return offset;
        }

        let offset = self.clamp_to_boundary(offset);
        grapheme_count(&self.content[..offset]) * "•".len()
    }

    fn content_offset_for_display_offset(&self, offset: usize) -> usize {
        if !self.masked {
            return offset.min(self.content.len());
        }

        let grapheme_index = offset / "•".len();
        self.content
            .grapheme_indices(true)
            .nth(grapheme_index)
            .map(|(index, _)| index)
            .unwrap_or(self.content.len())
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let anchor = if self.selection_reversed {
            self.selected_range.end
        } else {
            self.selected_range.start
        };
        let offset = self.clamp_to_boundary(offset);
        if offset < anchor {
            self.selected_range = offset..anchor;
            self.selection_reversed = true;
        } else {
            self.selected_range = anchor..offset;
            self.selection_reversed = false;
        }
        self.note_cursor_activity();
        cx.notify()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        utf8_offset_from_utf16(&self.content, offset)
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        utf16_offset_from_utf8(&self.content, offset)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        let start = self.clamp_to_boundary(range.start);
        let end = self.clamp_to_boundary(range.end);
        if start <= end {
            range_to_utf16_in_text(&self.content, &(start..end))
        } else {
            range_to_utf16_in_text(&self.content, &(end..start))
        }
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        let start = self.clamp_to_boundary(self.offset_from_utf16(range_utf16.start));
        let end = self.clamp_to_boundary(self.offset_from_utf16(range_utf16.end));
        if start <= end { start..end } else { end..start }
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        let offset = self.clamp_to_boundary(offset);
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        let offset = self.clamp_to_boundary(offset);
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    fn clamp_to_boundary(&self, offset: usize) -> usize {
        clamp_utf8_boundary(&self.content, offset)
    }
}

impl EventEmitter<EditorEvent> for Editor {}

impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.marked_range = None;
        self.note_cursor_activity();
        if self.composition_snapshot.take().is_some() {
            self.push_history_snapshot();
        }
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        range.start = self.clamp_to_boundary(range.start);
        range.end = self.clamp_to_boundary(range.end);
        if range.start > range.end {
            range = range.end..range.start;
        }
        let new_text = self.replacement_for_range(&range, new_text);

        let replacement =
            self.content[0..range.start].to_owned() + &new_text + &self.content[range.end..];
        if replacement == self.content.as_ref() {
            let cursor = range.start + new_text.len();
            self.selected_range = cursor..cursor;
            self.selection_reversed = false;
            self.note_cursor_activity();
            self.marked_range = None;
            let had_composition = self.composition_snapshot.take().is_some();
            if had_composition {
                self.push_history_snapshot();
            }
            cx.notify();
            return;
        }

        self.content = replacement.into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.selection_reversed = false;
        self.note_cursor_activity();
        self.marked_range.take();
        self.composition_snapshot = None;
        self.push_history_snapshot();
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        range.start = self.clamp_to_boundary(range.start);
        range.end = self.clamp_to_boundary(range.end);
        if range.start > range.end {
            range = range.end..range.start;
        }
        let new_text = self.replacement_for_range(&range, new_text);

        if self.composition_snapshot.is_none() {
            self.composition_snapshot = Some(self.snapshot());
        }

        let replacement =
            self.content[0..range.start].to_owned() + &new_text + &self.content[range.end..];
        self.content = replacement.into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = selected_range_for_marked_text(
            range.start,
            &new_text,
            new_selected_range_utf16.as_ref(),
        );
        self.selection_reversed = false;
        self.note_cursor_activity();

        // Marked text is an in-progress IME composition. It is deliberately
        // kept out of the undo stack so the completed composition is undone as
        // one edit when the platform later calls `replace_text_in_range`.
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let display_start = self.display_offset_for_content_offset(range.start);
        let display_end = self.display_offset_for_content_offset(range.end);
        Some(Bounds::from_corners(
            point(
                bounds.left() + last_layout.x_for_index(display_start) - self.scroll_offset,
                bounds.top(),
            ),
            point(
                bounds.left() + last_layout.x_for_index(display_end) - self.scroll_offset,
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;
        let display_index = last_layout.index_for_x(line_point.x + self.scroll_offset)?;
        let content_index = self.content_offset_for_display_offset(display_index);
        Some(self.offset_to_utf16(content_index))
    }
}

/// The GPUI element responsible for painting an [`Editor`] buffer.
pub(crate) struct EditorElement {
    input: Entity<Editor>,
}

pub(crate) struct PrepaintState {
    line: Option<ShapedLine>,
    scroll_offset: Pixels,
    cursor: Option<PaintQuad>,
    show_cursor: bool,
    selection: Option<PaintQuad>,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let theme = theme::get(cx);
        let focused = input.focus_handle.is_focused(window);
        let raw_content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let display_cursor = input.display_offset_for_content_offset(cursor);
        let display_selected_range = input.display_offset_for_content_offset(selected_range.start)
            ..input.display_offset_for_content_offset(selected_range.end);
        let display_marked_range = input.marked_range.as_ref().map(|range| {
            input.display_offset_for_content_offset(range.start)
                ..input.display_offset_for_content_offset(range.end)
        });
        let style = window.text_style();

        let display_text: SharedString = if raw_content.is_empty() {
            input.placeholder.clone()
        } else if input.masked {
            "•".repeat(grapheme_count(&raw_content)).into()
        } else {
            raw_content.clone()
        };
        let text_color = if raw_content.is_empty() {
            theme.placeholder_foreground
        } else {
            style.color
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked_range) = display_marked_range.as_ref() {
            vec![
                TextRun {
                    len: marked_range.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked_range.end - marked_range.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len().saturating_sub(marked_range.end),
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text, font_size, &runs, None);

        let scroll_offset = if input.centered && line.width < bounds.size.width {
            // Keep the same offset for painting, cursor placement, selection and
            // mouse hit testing. A negative offset centers a short numeric value.
            (line.width - bounds.size.width) / 2.0
        } else if raw_content.is_empty() {
            px(0.0)
        } else {
            Editor::scroll_offset_for_cursor(
                &line,
                bounds.size.width,
                display_cursor,
                input.scroll_offset,
            )
        };

        let cursor_pos = line.x_for_index(display_cursor.min(line.text.len())) - scroll_offset;
        // Zed's default `CursorShape::Bar` is a crisp, full-line two-pixel
        // caret. Snap its rectangle to physical pixels so it stays sharp on
        // fractional display scales and does not shimmer while scrolling.
        let cursor = cursor_bounds(bounds, cursor_pos, window.scale_factor());
        let show_cursor = input.cursor_is_visible(focused, Instant::now());
        let (selection, cursor) = if selected_range.is_empty() || raw_content.is_empty() {
            (None, Some(fill(cursor, theme.input_border_focused)))
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            bounds.left() + line.x_for_index(display_selected_range.start)
                                - scroll_offset,
                            bounds.top(),
                        ),
                        point(
                            bounds.left() + line.x_for_index(display_selected_range.end)
                                - scroll_offset,
                            bounds.bottom(),
                        ),
                    ),
                    theme.selection_background,
                )),
                None,
            )
        };
        PrepaintState {
            line: Some(line),
            scroll_offset,
            cursor,
            show_cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        let focused = focus_handle.is_focused(window);
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection)
        }
        let line = prepaint.line.take().unwrap();
        line.paint(
            point(bounds.left() - prepaint.scroll_offset, bounds.top()),
            window.line_height(),
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        )
        .unwrap();

        if focused
            && prepaint.show_cursor
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        self.input.update(cx, |input, cx| {
            input.sync_cursor_blink(focused, cx);
            input.scroll_offset = prepaint.scroll_offset;
            input.update_scroll_offset(&line, bounds.size.width);
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

fn password_visibility_icon(masked: bool) -> &'static str {
    if masked {
        "icons/eye-off.svg"
    } else {
        "icons/eye.svg"
    }
}

fn editor_suffix_icon(
    id: &'static str,
    icon_path: &'static str,
    icon_size: Pixels,
    theme: &theme::TinyTheme,
) -> gpui::Stateful<gpui::Div> {
    let hover_background = theme.secondary_hover;
    div()
        .id(id)
        .flex()
        .flex_none()
        .size(px(24.0))
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor_pointer()
        .hover(move |style| style.bg(hover_background))
        .child(
            svg()
                .path(icon_path)
                .size(icon_size)
                .text_color(theme.muted_foreground),
        )
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            // Keep the editor focused and prevent this click from moving its
            // selection through the editor's root mouse handler.
            window.prevent_default();
            cx.stop_propagation();
        })
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);
        let focused = self.focus_handle.is_focused(window);
        let mask_toggle_icon = password_visibility_icon(self.masked);
        let border_color = if self.borderless {
            theme.input_background
        } else if focused {
            theme.input_border_focused
        } else {
            theme.input_border
        };

        div()
            .flex()
            .key_context("Editor")
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_to_beginning))
            .on_action(cx.listener(Self::delete_to_end))
            .on_action(cx.listener(Self::delete_previous_word))
            .on_action(cx.listener(Self::delete_next_word))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::previous_word))
            .on_action(cx.listener(Self::next_word))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_previous_word))
            .on_action(cx.listener(Self::select_next_word))
            .on_action(cx.listener(Self::select_to_start))
            .on_action(cx.listener(Self::select_to_end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::move_to_start))
            .on_action(cx.listener(Self::move_to_end))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::escape))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::submit))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .items_center()
            .h(self.input_height)
            .w_full()
            .rounded(px(if self.compact { 6.0 } else { 8.0 }))
            .border_1()
            .border_color(border_color)
            .bg(theme.input_background)
            .px(px(if self.compact { 4.0 } else { 8.0 }))
            .gap_1()
            .text_color(theme.foreground)
            .text_sm()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(EditorElement { input: cx.entity() }),
            )
            .when(self.mask_toggle, |this| {
                this.child(
                    editor_suffix_icon(
                        "toggle-password-visibility",
                        mask_toggle_icon,
                        px(16.0),
                        theme,
                    )
                    .on_click(cx.listener(Self::mask_toggle_button)),
                )
            })
            .when(self.clearable && !self.content.is_empty(), |this| {
                this.child(
                    editor_suffix_icon(
                        "text-input-clear-button",
                        "icons/window-close.svg",
                        px(14.0),
                        theme,
                    )
                    .on_click(cx.listener(Self::clear_button)),
                )
            })
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_suffix_icon_reflects_mask_state() {
        assert_eq!(password_visibility_icon(true), "icons/eye-off.svg");
        assert_eq!(password_visibility_icon(false), "icons/eye.svg");
    }

    #[test]
    fn converts_between_utf8_and_utf16_offsets() {
        let text = "a😀é中";

        assert_eq!(utf8_offset_from_utf16(text, 0), 0);
        assert_eq!(utf8_offset_from_utf16(text, 1), 1);
        assert_eq!(utf8_offset_from_utf16(text, 2), 5);
        assert_eq!(utf8_offset_from_utf16(text, 3), 5);
        assert_eq!(utf8_offset_from_utf16(text, 6), text.len());

        assert_eq!(utf16_offset_from_utf8(text, 0), 0);
        assert_eq!(utf16_offset_from_utf8(text, 1), 1);
        assert_eq!(utf16_offset_from_utf8(text, 5), 3);
        assert_eq!(utf16_offset_from_utf8(text, text.len()), 6);
    }

    #[test]
    fn converts_ranges_between_utf8_and_utf16() {
        let text = "a😀bc";

        assert_eq!(range_from_utf16_in_text(text, &(1..4)), 1..6);
        assert_eq!(range_to_utf16_in_text(text, &(1..6)), 1..4);
    }

    #[test]
    fn marked_text_selection_is_relative_to_inserted_text() {
        let inserted_at = "ab".len();
        let inserted_text = "拼音";

        let selected = selected_range_for_marked_text(inserted_at, inserted_text, Some(&(1..2)));

        assert_eq!(
            selected,
            inserted_at + "拼".len()..inserted_at + inserted_text.len()
        );
    }

    #[test]
    fn marked_text_selection_defaults_to_inserted_text_end() {
        let inserted_at = "prefix".len();
        let inserted_text = "候选";

        let selected = selected_range_for_marked_text(inserted_at, inserted_text, None);

        assert_eq!(
            selected,
            inserted_at + inserted_text.len()..inserted_at + inserted_text.len()
        );
    }

    #[test]
    fn marked_text_selection_clamps_to_inserted_text() {
        let inserted_at = 3;
        let inserted_text = "😀";

        let selected = selected_range_for_marked_text(inserted_at, inserted_text, Some(&(0..10)));

        assert_eq!(selected, inserted_at..inserted_at + inserted_text.len());
    }

    #[test]
    fn marked_text_selection_normalizes_reversed_ranges() {
        let reversed_range = Range { start: 2, end: 1 };
        let selected = selected_range_for_marked_text(2, "拼音", Some(&reversed_range));

        assert_eq!(selected, 2 + "拼".len()..2 + "拼音".len());
    }

    #[test]
    fn filters_single_line_input_before_applying_limits() {
        assert_eq!(filter_editor_text("12\n3\r4a", true, Some(3)), "123");
        assert_eq!(
            filter_editor_text("标题\n第二行\r\n", false, None),
            "标题 第二行 "
        );
        assert_eq!(filter_editor_text("é😀", false, Some(1)), "é");
    }

    #[test]
    fn word_boundaries_match_single_line_editor_movement() {
        let text = "foo.bar baz";

        assert_eq!(previous_word_boundary_in_text(text, text.len()), 8);
        assert_eq!(previous_word_boundary_in_text(text, 8), 4);
        assert_eq!(next_word_boundary_in_text(text, 0), 3);
        assert_eq!(next_word_boundary_in_text(text, 3), 7);
        assert_eq!(next_word_boundary_in_text(text, 7), text.len());
    }

    #[test]
    fn double_click_ranges_cover_words_and_punctuation() {
        let text = "foo.bar  baz";

        assert_eq!(word_range_at(text, 1), 0..3);
        assert_eq!(word_range_at(text, 3), 3..4);
        assert_eq!(word_range_at(text, 5), 4..7);
        assert_eq!(word_range_at(text, text.len()), 9..text.len());
    }

    #[test]
    fn grapheme_offsets_are_never_left_inside_a_character() {
        let text = "a😀é";

        assert_eq!(clamp_utf8_boundary(text, 2), 1);
        assert_eq!(clamp_utf8_boundary(text, 4), 5);
    }

    #[test]
    fn cursor_blink_cycle_matches_zed_cadence() {
        assert!(cursor_visible_after(Duration::ZERO));
        assert!(cursor_visible_after(Duration::from_millis(499)));
        assert!(!cursor_visible_after(Duration::from_millis(500)));
        assert!(!cursor_visible_after(Duration::from_millis(999)));
        assert!(cursor_visible_after(Duration::from_millis(1_000)));
    }

    #[test]
    fn cursor_bounds_use_a_snapped_full_height_bar() {
        let field = Bounds::new(point(px(10.25), px(20.25)), size(px(100.0), px(18.0)));
        let cursor = cursor_bounds(field, px(24.75), 1.0);

        assert_eq!(cursor.origin.x, px(35.0));
        assert_eq!(cursor.origin.y, px(20.0));
        assert_eq!(cursor.size.width, px(2.0));
        assert_eq!(cursor.size.height, px(18.0));
    }

    #[test]
    fn cursor_bounds_stay_inside_narrow_fields() {
        let field = Bounds::new(point(px(4.0), px(8.0)), size(px(1.0), px(12.0)));
        let cursor = cursor_bounds(field, px(40.0), 1.0);

        assert_eq!(cursor.origin.x, px(4.0));
        assert_eq!(cursor.size.width, px(1.0));
        assert_eq!(cursor.size.height, px(12.0));
    }
}
