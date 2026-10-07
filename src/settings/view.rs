//! Settings window presentation; pure settings policy remains in the controller.
mod about;
pub(crate) mod controls;
mod development;
mod dialog;
mod user;

#[cfg(test)]
pub(crate) mod test_support;

pub(crate) use dialog::{SettingsChanged, SettingsDialogMode, SettingsDialogState};
