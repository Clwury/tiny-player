use crate::effects::EffectHandle;

/// View resources live with HomeContent; business state lives in HomeController.
#[derive(Debug, Default)]
pub(in crate::home) struct LibraryResources {
    pub(in crate::home) effect: EffectHandle<gpui::Task<()>>,
    pub(in crate::home) presentation: LibraryPresentation,
}

#[derive(Debug, Default)]
pub(in crate::home) struct LibraryPresentation {
    pub(in crate::home) sort_menu_open: bool,
    pub(in crate::home) grid: crate::home::presentation::GridPresentation,
}

pub(in crate::home) struct LibraryView<'a> {
    pub(in crate::home) model: super::controller::LibraryVm<'a>,
    pub(in crate::home) presentation: &'a LibraryPresentation,
}
