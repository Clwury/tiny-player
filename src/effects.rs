//! Shared request identity and cancellation protocol, independent of executors.
use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::observability::TraceId;

/// A controller's account identity. Created at workspace entry, immutable for
/// that workspace; request tokens retain it until the effect is released.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub(crate) struct WorkspaceIdentity {
    pub(crate) local_server_id: String,
    pub(crate) remote_server_id: Option<String>,
    pub(crate) user_id: Option<String>,
}

impl fmt::Debug for WorkspaceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkspaceIdentity").finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RequestScope {
    ItemImage {
        key: crate::images::cache::CachedImageKey,
    },
    HomeResize,
    HomeDashboardWarmup,
    AppHomeMount,
    AppPlaybackMount,
    NotificationHide {
        id: u64,
    },
    EditorCaretBlink,
    DropdownFocus,
    PlaybackBackendPoll,
    PlaybackQueue,
    PlaybackReportTimer,
    PlaybackReportStart,
    PlaybackReportProgress,
    PlaybackReportStop,
    PlaybackControlsHide,
    PlaybackVolumeHide,
    PlaybackRateHide,
    PlaybackSpeedRefresh,
    HomeSnapshot,
    UserViews,
    ResumeItems,
    LatestItems {
        view_id: String,
    },
    HomeCachedImages,
    HomeNetworkRefresh,
    HomePlaybackRefresh,
    Persistence,
    ServerAuth,
    ServerSave,
    ServerIcon,
    ServerCounts {
        server_id: String,
    },
    Search,
    Library {
        view_id: String,
    },
    Person {
        person_id: String,
    },
    PersonItems {
        person_id: String,
    },
    GenreItems {
        genre_key: String,
    },
    Favorites {
        item_type: crate::home::FavoriteItemType,
    },
    FavoriteOverview {
        item_type: crate::home::FavoriteItemType,
    },
    FavoriteMutation {
        item_id: String,
    },
    ResumeMutation {
        item_id: String,
    },
    PlayedMutation {
        item_id: String,
    },
    Detail {
        item_id: String,
        resource: DetailResource,
    },
    DetailActivation {
        item_id: String,
    },
    DetailPlayback {
        item_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DetailResource {
    Item,
    Similar,
    Seasons,
    NextUp,
    Episodes,
    ResumeSources,
}

impl RequestScope {
    fn operation(&self) -> &'static str {
        match self {
            Self::ItemImage { .. } => "request.item_image",
            Self::HomeResize => "request.home_resize",
            Self::HomeDashboardWarmup => "request.home_dashboard_warmup",
            Self::AppHomeMount => "request.app_home_mount",
            Self::AppPlaybackMount => "request.app_playback_mount",
            Self::NotificationHide { .. } => "request.notification_hide",
            Self::EditorCaretBlink => "request.editor_caret_blink",
            Self::DropdownFocus => "request.dropdown_focus",
            Self::PlaybackBackendPoll => "request.playback_backend_poll",
            Self::PlaybackQueue => "request.playback_queue",
            Self::PlaybackReportTimer => "request.playback_report_timer",
            Self::PlaybackReportStart => "request.playback_report_start",
            Self::PlaybackReportProgress => "request.playback_report_progress",
            Self::PlaybackReportStop => "request.playback_report_stop",
            Self::PlaybackControlsHide => "request.playback_controls_hide",
            Self::PlaybackVolumeHide => "request.playback_volume_hide",
            Self::PlaybackRateHide => "request.playback_rate_hide",
            Self::PlaybackSpeedRefresh => "request.playback_speed_refresh",
            Self::HomeSnapshot => "request.home_snapshot",
            Self::UserViews => "request.user_views",
            Self::ResumeItems => "request.resume_items",
            Self::LatestItems { .. } => "request.latest_items",
            Self::HomeCachedImages => "request.home_cached_images",
            Self::HomeNetworkRefresh => "request.home_network_refresh",
            Self::HomePlaybackRefresh => "request.home_playback_refresh",
            Self::Persistence => "request.persistence",
            Self::ServerAuth => "request.server_auth",
            Self::ServerSave => "request.server_save",
            Self::ServerIcon => "request.server_icon",
            Self::ServerCounts { .. } => "request.server_counts",
            Self::Search => "request.search",
            Self::Library { .. } => "request.library",
            Self::Person { .. } => "request.person",
            Self::PersonItems { .. } => "request.person_items",
            Self::GenreItems { .. } => "request.genre_items",
            Self::Favorites { .. } => "request.favorites",
            Self::FavoriteOverview { .. } => "request.favorite_overview",
            Self::FavoriteMutation { .. } => "request.favorite_mutation",
            Self::ResumeMutation { .. } => "request.resume_mutation",
            Self::PlayedMutation { .. } => "request.played_mutation",
            Self::Detail { .. } => "request.detail",
            Self::DetailActivation { .. } => "request.detail_activation",
            Self::DetailPlayback { .. } => "request.detail_playback",
        }
    }
}

/// Owned by the effect until completion. IDs and generations are opaque so a
/// feature cannot accidentally compare a search counter to a pagination one.
#[derive(Clone, Debug)]
pub(crate) struct RequestToken {
    owner: u64,
    scope: RequestScope,
    generation: u64,
    identity: WorkspaceIdentity,
    trace: TraceId,
}

impl PartialEq for RequestToken {
    fn eq(&self, other: &Self) -> bool {
        self.owner == other.owner
            && self.scope == other.scope
            && self.generation == other.generation
            && self.identity == other.identity
    }
}
impl Eq for RequestToken {}

impl RequestToken {
    pub(crate) fn is_current(&self, slot: &RequestSlot) -> bool {
        let current = slot.active && slot.last.as_ref() == Some(self);
        if !current {
            self.trace.record("discarded");
        }
        current
    }

    pub(crate) fn is_for(&self, identity: &WorkspaceIdentity) -> bool {
        let matches = self.identity == *identity;
        if !matches {
            self.trace.record("discarded_identity");
        }
        matches
    }
}

/// Feature-owned scope state. issue replaces the current request; invalidate
/// handles query/route changes; commit consumes a result once. Dropped with owner.
#[derive(Debug)]
pub(crate) struct RequestSlot {
    owner: u64,
    scope: RequestScope,
    identity: WorkspaceIdentity,
    generation: u64,
    last: Option<RequestToken>,
    active: bool,
}

impl RequestSlot {
    pub(crate) fn new(scope: RequestScope, identity: WorkspaceIdentity) -> Self {
        static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);
        Self {
            owner: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
            scope,
            identity,
            generation: 0,
            last: None,
            active: false,
        }
    }

    pub(crate) fn issue(&mut self) -> RequestToken {
        self.invalidate();
        self.generation = self.generation.wrapping_add(1);
        let token = RequestToken {
            owner: self.owner,
            scope: self.scope.clone(),
            generation: self.generation,
            identity: self.identity.clone(),
            trace: TraceId::start(self.scope.operation()),
        };
        self.last = Some(token.clone());
        self.active = true;
        token
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    pub(crate) fn invalidate(&mut self) {
        if self.active
            && let Some(token) = &self.last
        {
            token.trace.record("cancelled");
        }
        self.active = false;
    }

    pub(crate) fn commit(&mut self, token: &RequestToken) -> bool {
        if !token.is_current(self) {
            return false;
        }
        self.active = false;
        token.trace.record("committed");
        true
    }

    #[cfg(test)]
    pub(crate) fn latest(&self) -> Option<RequestToken> {
        self.last.clone()
    }
}

impl Drop for RequestSlot {
    fn drop(&mut self) {
        self.invalidate();
    }
}

/// Runner-owned cancellation handle. Replacing/dropping a GPUI Task cancels
/// its continuation. Blocking IO may finish, so reducers must still check tokens.
#[derive(Debug)]
pub(crate) struct EffectHandle<H> {
    handle: Option<H>,
}

impl<H> Default for EffectHandle<H> {
    fn default() -> Self {
        Self { handle: None }
    }
}

impl<H> EffectHandle<H> {
    pub(crate) fn replace(&mut self, handle: H) {
        self.handle = Some(handle);
    }
    pub(crate) fn cancel(&mut self) {
        self.handle = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_invalidation_and_completion_reject_old_tokens() {
        let mut slot = RequestSlot::new(RequestScope::Search, WorkspaceIdentity::default());
        let first = slot.issue();
        let next = slot.issue();
        assert!(!first.is_current(&slot));
        assert!(next.is_current(&slot));
        assert!(slot.commit(&next));
        assert!(!slot.commit(&next));
        let last = slot.issue();
        slot.invalidate();
        assert!(!last.is_current(&slot));
    }

    #[test]
    fn same_generation_from_another_owner_or_account_is_not_current() {
        let identity = WorkspaceIdentity {
            local_server_id: "local".into(),
            remote_server_id: Some("remote".into()),
            user_id: Some("user".into()),
        };
        let mut slot = RequestSlot::new(RequestScope::Search, identity.clone());
        let token = slot.issue();
        let mut other = RequestSlot::new(RequestScope::Search, identity.clone());
        let other_token = other.issue();
        assert!(!other_token.is_current(&slot));
        assert!(token.is_for(&identity));
        for changed in [
            WorkspaceIdentity {
                local_server_id: "other".into(),
                ..identity.clone()
            },
            WorkspaceIdentity {
                remote_server_id: None,
                ..identity.clone()
            },
            WorkspaceIdentity {
                user_id: None,
                ..identity.clone()
            },
        ] {
            assert!(!token.is_for(&changed));
        }
    }

    #[test]
    fn replacing_cancelling_and_releasing_handles_drop_the_task() {
        use std::{cell::Cell, rc::Rc};
        struct Task(Rc<Cell<usize>>);
        impl Drop for Task {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let dropped = Rc::new(Cell::new(0));
        let mut handle = EffectHandle::default();
        handle.replace(Task(dropped.clone()));
        handle.replace(Task(dropped.clone()));
        assert_eq!(dropped.get(), 1);
        handle.cancel();
        handle.cancel();
        assert_eq!(dropped.get(), 2);
        handle.replace(Task(dropped.clone()));
        drop(handle);
        assert_eq!(dropped.get(), 3);
    }
}
