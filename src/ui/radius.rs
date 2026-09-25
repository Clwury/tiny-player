//! Component corners follow Zed's UI scale, independently of window decorations.
//! Rem units match GPUI's rounded_sm/md/lg and scale with the UI font size.

use gpui::{Rems, rems};

/// Buttons, select triggers, menu items, navigation rows, and tags (4px at 16px/rem).
pub(crate) const CONTROL: Rems = rems(0.25);
/// Text fields and joined number controls (6px at 16px/rem).
pub(crate) const INPUT: Rems = rems(0.375);
/// Cards, their images, placeholders, and selection outlines (6px at 16px/rem).
pub(crate) const CARD: Rems = rems(0.375);
/// Dialogs, notifications, menus, popovers, and tooltips (8px at 16px/rem).
pub(crate) const SURFACE: Rems = rems(0.5);
