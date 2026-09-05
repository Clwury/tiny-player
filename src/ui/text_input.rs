//! Backwards-compatible input module.
//!
//! New code should use the `super::editor::Editor` module. The old names stay
//! available so integrations built against the original tiny-player input
//! widget keep compiling while they migrate.

pub(crate) use super::editor::{Editor, EditorEvent};

#[allow(dead_code)]
pub(crate) type TextInput = Editor;

#[allow(dead_code)]
pub(crate) type TextInputEvent = EditorEvent;
