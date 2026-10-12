//! Application text is localized at presentation boundaries. Models keep their
//! canonical messages; server-provided media names and descriptions stay intact.
mod binding;
mod catalog;
mod language;

pub(crate) use binding::tr;
pub(crate) use catalog::translate;
pub(crate) use language::AppLanguage;
