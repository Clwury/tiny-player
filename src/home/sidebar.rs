//! Sidebar feature API; policy, GPUI event binding and view have separate owners.
mod binding;
pub(in crate::home) mod controller;
pub(super) mod reorder;
mod view;

pub(in crate::home) use view::{SidebarListener, SidebarProps, SidebarViewIntent, render_sidebar};
