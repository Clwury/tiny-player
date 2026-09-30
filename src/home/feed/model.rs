use std::collections::HashMap;

use crate::{
    effects::RequestToken,
    emby::{ResumeItems, UserItem, UserItems, UserViews, VideoItemType},
    home::{cache::HomeSnapshot, model::LoadState},
};

#[derive(Clone, Debug, Default)]
pub(crate) struct UserViewItemsRow {
    pub(crate) items: Option<UserItems>,
    pub(crate) loading: bool,
}

/// FeedController owns dashboard data. Its reducers write load results and
/// hydration; HomeController coordinates shared user-data mutations and playback
/// updates. GPUI readers receive borrowed selectors, never this mutable state.
/// Refresh clears content. Route returns reuse it; workspace release drops it.
#[derive(Debug, Default)]
pub(in crate::home) struct FeedState {
    pub(in crate::home) home_snapshot: LoadState,
    pub(in crate::home) views_load: LoadState,
    pub(in crate::home) resume_load: LoadState,
    pub(in crate::home) user_views: Option<UserViews>,
    pub(in crate::home) user_views_failed: Option<String>,
    pub(in crate::home) resume_items: Option<ResumeItems>,
    pub(in crate::home) resume_items_failed: Option<String>,
    pub(in crate::home) user_view_items_rows: HashMap<String, UserViewItemsRow>,
}

#[derive(Clone, Copy, Debug)]
pub(in crate::home) enum FeedIntent {
    LoadSnapshot,
    LoadViews,
    LoadResume,
    Refresh,
    RefreshResume,
    LoadLatest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::home) enum FeedRequestKind {
    Snapshot,
    Views,
    Resume,
    Latest {
        view_id: String,
        item_types: Vec<VideoItemType>,
    },
}

#[derive(Clone, Debug)]
pub(in crate::home) struct FeedRequest {
    pub(in crate::home) kind: FeedRequestKind,
    pub(in crate::home) token: RequestToken,
}

pub(in crate::home) enum FeedResponse {
    Snapshot(anyhow::Result<Option<Box<HomeSnapshot>>>),
    Views(anyhow::Result<UserViews>),
    Resume(anyhow::Result<ResumeItems>),
    Latest(anyhow::Result<Vec<UserItem>>),
}

pub(in crate::home) enum FeedUpdate {
    Ignored,
    Snapshot(Option<Box<HomeSnapshot>>),
    Views,
    Resume,
    Latest(String),
    Failed {
        kind: FeedRequestKind,
        message: String,
    },
}
