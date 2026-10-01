//! Settings window presentation; pure settings policy remains in the controller.
pub(crate) mod controls;
mod development;
mod dialog;
mod user;

#[cfg(test)]
pub(crate) mod test_support;

pub(crate) use dialog::{SettingsChanged, SettingsDialogMode, SettingsDialogState};
