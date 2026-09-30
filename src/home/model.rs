//! Pure Home state and transitions. No GPUI entities, handles, IO or tasks.

pub(crate) mod cards;
pub(crate) mod detail;
pub(crate) mod layout;
pub(crate) mod library;
pub(crate) mod navigation;
pub(crate) mod notification;
pub(crate) mod paged_items;
pub(crate) mod search;
pub(crate) mod sidebar;
pub(crate) mod user_data;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LoadState {
    #[default]
    Idle,
    Loading,
    Loaded,
    Failed,
}

impl LoadState {
    pub(crate) fn can_start(self) -> bool {
        matches!(self, Self::Idle | Self::Failed)
    }

    pub(crate) fn is_loading(self) -> bool {
        matches!(self, Self::Loading)
    }
}

pub(crate) fn favorite_section_title(item_type: crate::emby::VideoItemType) -> &'static str {
    match item_type {
        crate::emby::VideoItemType::Movie => "电影",
        crate::emby::VideoItemType::Series => "剧集",
        crate::emby::VideoItemType::Episode => "集",
    }
}
