//! Pure resize and dashboard-cache policy. The page supplies bounds, route and
//! monotonic time; it owns timer/frame delivery and all GPUI resources.
use std::time::{Duration, Instant};

use super::navigation::{HomeRoot, HomeRoute};
use crate::effects::{RequestScope, RequestSlot, RequestToken, WorkspaceIdentity};

pub(in crate::home) const RESIZE_SETTLE_DEBOUNCE: Duration = Duration::from_millis(120);
const HOME_ITEM_RENDER_OVERSCAN_BEFORE: usize = 2;
const HOME_ITEM_RENDER_OVERSCAN_AFTER: usize = 4;
const WORKSPACE_GRID_ABRUPT_COLUMN_DELTA: usize = 2;

/// One workspace owns this state. Bounds extend the current burst rather than
/// allocating per-frame tasks. A new burst invalidates pending warmup frames;
/// dropping the controller invalidates both request scopes.
#[derive(Debug)]
pub(in crate::home) struct HomeLayoutController {
    last_window_size: Option<(u32, u32)>,
    burst: Option<ResizeBurst>,
    settle: RequestSlot,
    warmup_slot: RequestSlot,
    warmup: Option<RequestToken>,
    defer_contraction: bool,
    grid_columns: usize,
    reuse_dashboard_until_frame: bool,
    dashboard_dirty: bool,
}

#[derive(Debug)]
struct ResizeBurst {
    last_activity: Instant,
    warm_dashboard: bool,
}

pub(in crate::home) struct LayoutFrame {
    pub(in crate::home) resized: bool,
    pub(in crate::home) start_settle: Option<RequestToken>,
}

pub(in crate::home) enum ResizeTick {
    Ignored,
    Wait(Duration),
    Settled { warmup: Option<RequestToken> },
}

#[derive(Clone, Copy, Debug)]
pub(in crate::home) struct HomeLayoutVm {
    pub(in crate::home) grid_columns: usize,
    pub(in crate::home) carousel_overscan: (usize, usize),
    pub(in crate::home) grid_overscan_rows: usize,
    pub(in crate::home) auto_paginate: bool,
    resizing: bool,
    warmup_deferred: bool,
}

impl HomeLayoutVm {
    pub(in crate::home) fn dashboard_should_mount(
        self,
        route: &HomeRoute,
        has_authentication_error: bool,
    ) -> bool {
        home_dashboard_should_mount(
            route,
            has_authentication_error,
            self.resizing,
            self.warmup_deferred,
        )
    }
}

impl HomeLayoutController {
    pub(in crate::home) fn new(identity: WorkspaceIdentity) -> Self {
        Self {
            last_window_size: None,
            burst: None,
            settle: RequestSlot::new(RequestScope::HomeResize, identity.clone()),
            warmup_slot: RequestSlot::new(RequestScope::HomeDashboardWarmup, identity),
            warmup: None,
            defer_contraction: false,
            grid_columns: 1,
            reuse_dashboard_until_frame: false,
            dashboard_dirty: false,
        }
    }

    pub(in crate::home) fn prepare_frame(
        &mut self,
        size: (u32, u32),
        measured_columns: usize,
        route: &HomeRoute,
        now: Instant,
    ) -> LayoutFrame {
        let resized = self
            .last_window_size
            .replace(size)
            .is_some_and(|previous| previous != size);
        let mut start_settle = None;
        if resized {
            self.warmup_slot.invalidate();
            self.warmup = None;
            if self.burst.is_none() {
                start_settle = Some(self.settle.issue());
            }
            self.burst = Some(ResizeBurst {
                last_activity: now,
                warm_dashboard: route != &HomeRoute::Root(HomeRoot::Home),
            });
            if matches!(
                route,
                HomeRoute::FavoriteItems { .. } | HomeRoute::Root(HomeRoot::Search)
            ) && workspace_grid_contraction_is_abrupt(self.grid_columns, measured_columns)
            {
                self.defer_contraction = true;
            }
        }
        self.grid_columns = workspace_grid_columns_during_resize(
            self.grid_columns,
            measured_columns,
            self.defer_contraction,
        );
        LayoutFrame {
            resized,
            start_settle,
        }
    }

    pub(in crate::home) fn resize_tick(
        &mut self,
        token: &RequestToken,
        identity: &WorkspaceIdentity,
        route: &HomeRoute,
        has_authentication_error: bool,
        now: Instant,
    ) -> ResizeTick {
        if !token.is_for(identity) || !token.is_current(&self.settle) {
            return ResizeTick::Ignored;
        }
        let Some(burst) = &self.burst else {
            return ResizeTick::Ignored;
        };
        let remaining = RESIZE_SETTLE_DEBOUNCE
            .saturating_sub(now.saturating_duration_since(burst.last_activity));
        if !remaining.is_zero() {
            return ResizeTick::Wait(remaining);
        }
        let warm_dashboard = burst.warm_dashboard
            && route != &HomeRoute::Root(HomeRoot::Home)
            && !has_authentication_error;
        self.settle.commit(token);
        self.burst = None;
        self.defer_contraction = false;
        self.warmup = warm_dashboard.then(|| self.warmup_slot.issue());
        ResizeTick::Settled {
            warmup: self.warmup.clone(),
        }
    }

    pub(in crate::home) fn complete_warmup(
        &mut self,
        token: &RequestToken,
        identity: &WorkspaceIdentity,
    ) -> bool {
        if !token.is_for(identity) || self.burst.is_some() || !self.warmup_slot.commit(token) {
            return false;
        }
        self.warmup = None;
        true
    }

    pub(in crate::home) fn view_model(&self) -> HomeLayoutVm {
        let resizing = self.burst.is_some();
        HomeLayoutVm {
            grid_columns: self.grid_columns,
            carousel_overscan: home_carousel_overscan(resizing),
            grid_overscan_rows: usize::from(!resizing),
            auto_paginate: !resizing,
            resizing,
            warmup_deferred: self.warmup.is_some(),
        }
    }

    pub(in crate::home) fn root_selected(&mut self, root: HomeRoot) {
        self.reuse_dashboard_until_frame = root == HomeRoot::Home;
    }

    pub(in crate::home) fn content_changed(&mut self) {
        self.dashboard_dirty = true;
    }

    pub(in crate::home) fn page_notified(&mut self, route: &HomeRoute) -> bool {
        // Data can arrive while Home is hidden or in the same cycle as a route
        // return. Such changes must take precedence over route-only reuse.
        if route != &HomeRoute::Root(HomeRoot::Home)
            || (self.reuse_dashboard_until_frame && !self.dashboard_dirty)
        {
            return false;
        }
        self.dashboard_dirty = false;
        true
    }

    pub(in crate::home) fn frame_prepared(&mut self) {
        self.reuse_dashboard_until_frame = false;
    }
}

fn home_dashboard_should_mount(
    route: &HomeRoute,
    has_authentication_error: bool,
    resize_in_progress: bool,
    warmup_deferred: bool,
) -> bool {
    !has_authentication_error
        && (route == &HomeRoute::Root(HomeRoot::Home) || (!resize_in_progress && !warmup_deferred))
}

fn workspace_grid_contraction_is_abrupt(current: usize, measured: usize) -> bool {
    current.max(1).saturating_sub(measured.max(1)) >= WORKSPACE_GRID_ABRUPT_COLUMN_DELTA
}

fn workspace_grid_columns_during_resize(
    current: usize,
    measured: usize,
    defer_contraction: bool,
) -> usize {
    let current = current.max(1);
    let measured = measured.max(1);
    if defer_contraction && measured < current {
        current
    } else {
        // Expansion remains live. Only an abrupt contraction is held until the
        // current resize burst settles.
        measured
    }
}

pub(in crate::home) fn home_carousel_overscan(resize_in_progress: bool) -> (usize, usize) {
    if resize_in_progress {
        (0, 0)
    } else {
        (
            HOME_ITEM_RENDER_OVERSCAN_BEFORE,
            HOME_ITEM_RENDER_OVERSCAN_AFTER,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> WorkspaceIdentity {
        WorkspaceIdentity {
            local_server_id: "local".into(),
            user_id: Some("user".into()),
            ..Default::default()
        }
    }

    fn settle(
        layout: &mut HomeLayoutController,
        token: &RequestToken,
        route: &HomeRoute,
        now: Instant,
    ) -> Option<RequestToken> {
        match layout.resize_tick(token, &identity(), route, false, now) {
            ResizeTick::Settled { warmup } => warmup,
            _ => panic!("resize should settle"),
        }
    }

    #[test]
    fn resize_burst_uses_one_token_and_settles_from_the_last_bounds_change() {
        let mut layout = HomeLayoutController::new(identity());
        let route = HomeRoute::Root(HomeRoot::Search);
        let now = Instant::now();
        let first = layout.prepare_frame((1600, 900), 7, &route, now);
        assert!(!first.resized);
        assert!(first.start_settle.is_none());
        let first = layout.prepare_frame((900, 900), 3, &route, now);
        let token = first.start_settle.unwrap();
        assert_eq!(layout.view_model().grid_columns, 7);
        assert_eq!(layout.view_model().carousel_overscan, (0, 0));
        assert_eq!(layout.view_model().grid_overscan_rows, 0);
        assert!(!layout.view_model().auto_paginate);
        assert!(!layout.view_model().dashboard_should_mount(&route, false));
        let next = layout.prepare_frame((1800, 1000), 8, &route, now + Duration::from_millis(70));
        assert!(next.resized);
        assert!(next.start_settle.is_none());
        assert_eq!(layout.view_model().grid_columns, 8);
        assert!(
            matches!(layout.resize_tick(&token, &identity(), &route, false, now + Duration::from_millis(120)), ResizeTick::Wait(wait) if wait == Duration::from_millis(70))
        );
        // Repainting unchanged bounds does not extend the deadline.
        assert!(
            !layout
                .prepare_frame((1800, 1000), 8, &route, now + Duration::from_millis(150))
                .resized
        );
        let warmup = settle(
            &mut layout,
            &token,
            &route,
            now + Duration::from_millis(190),
        )
        .unwrap();
        assert!(layout.view_model().auto_paginate);
        assert_eq!(layout.view_model().carousel_overscan, (2, 4));
        assert!(!layout.view_model().dashboard_should_mount(&route, false));
        assert!(matches!(
            layout.resize_tick(
                &token,
                &identity(),
                &route,
                false,
                now + Duration::from_secs(1)
            ),
            ResizeTick::Ignored
        ));
        assert!(layout.complete_warmup(&warmup, &identity()));
        assert!(layout.view_model().dashboard_should_mount(&route, false));
        assert!(!layout.complete_warmup(&warmup, &identity()));
    }

    #[test]
    fn new_resize_and_foreign_accounts_cannot_commit_old_settle_or_warmup() {
        let mut layout = HomeLayoutController::new(identity());
        let route = HomeRoute::Root(HomeRoot::Search);
        let now = Instant::now();
        layout.prepare_frame((1600, 900), 7, &route, now);
        let old = layout
            .prepare_frame((900, 900), 3, &route, now)
            .start_settle
            .unwrap();
        let foreign = WorkspaceIdentity {
            user_id: Some("other".into()),
            ..identity()
        };
        assert!(matches!(
            layout.resize_tick(&old, &foreign, &route, false, now + RESIZE_SETTLE_DEBOUNCE),
            ResizeTick::Ignored
        ));
        assert!(!layout.view_model().auto_paginate);
        let old_warmup = settle(&mut layout, &old, &route, now + RESIZE_SETTLE_DEBOUNCE).unwrap();
        assert!(!layout.complete_warmup(&old_warmup, &foreign));
        let new = layout
            .prepare_frame((800, 800), 2, &route, now + Duration::from_millis(130))
            .start_settle
            .unwrap();
        assert!(!layout.complete_warmup(&old_warmup, &identity()));
        assert!(matches!(
            layout.resize_tick(
                &old,
                &identity(),
                &route,
                false,
                now + Duration::from_secs(1)
            ),
            ResizeTick::Ignored
        ));
        let new_warmup = settle(&mut layout, &new, &route, now + Duration::from_secs(1)).unwrap();
        assert!(!layout.complete_warmup(&old_warmup, &identity()));
        assert!(!layout.view_model().dashboard_should_mount(&route, false));
        assert!(layout.complete_warmup(&new_warmup, &identity()));
    }

    #[test]
    fn settling_commits_held_columns_and_skips_warmup_after_home_return_or_auth_failure() {
        for (root, auth) in [
            (HomeRoot::Home, false),
            (HomeRoot::Search, true),
            (HomeRoot::Search, false),
        ] {
            let mut layout = HomeLayoutController::new(identity());
            let route = HomeRoute::Root(HomeRoot::Search);
            let now = Instant::now();
            layout.prepare_frame((1600, 900), 7, &route, now);
            let token = layout
                .prepare_frame((900, 900), 3, &route, now)
                .start_settle
                .unwrap();
            let current = HomeRoute::Root(root);
            assert!(
                matches!(layout.resize_tick(&token, &identity(), &current, auth, now + RESIZE_SETTLE_DEBOUNCE), ResizeTick::Settled { warmup } if warmup.is_some() == (root != HomeRoot::Home && !auth))
            );
            layout.prepare_frame((900, 900), 3, &current, now + RESIZE_SETTLE_DEBOUNCE);
            assert_eq!(layout.view_model().grid_columns, 3);
            assert_eq!(
                layout.view_model().dashboard_should_mount(&current, auth),
                root == HomeRoot::Home
            );
        }
    }

    #[test]
    fn dashboard_reuses_route_only_return_but_hidden_or_same_cycle_data_wins() {
        let mut layout = HomeLayoutController::new(identity());
        let home = HomeRoute::Root(HomeRoot::Home);
        let search = HomeRoute::Root(HomeRoot::Search);
        assert!(layout.page_notified(&home));
        assert!(!layout.page_notified(&search));
        layout.root_selected(HomeRoot::Home);
        assert!(!layout.page_notified(&home));
        layout.frame_prepared();
        assert!(layout.page_notified(&home));
        layout.content_changed();
        assert!(!layout.page_notified(&search));
        layout.root_selected(HomeRoot::Home);
        assert!(layout.page_notified(&home));
        assert!(!layout.page_notified(&home));
        layout.content_changed();
        assert!(layout.page_notified(&home));
    }
    #[test]
    fn home_dashboard_cache_stays_warm_except_during_non_home_resize() {
        assert!(home_dashboard_should_mount(
            &HomeRoute::Root(HomeRoot::Home),
            false,
            false,
            false,
        ));
        assert!(home_dashboard_should_mount(
            &HomeRoute::Root(HomeRoot::Favorites),
            false,
            false,
            false,
        ));
        assert!(!home_dashboard_should_mount(
            &HomeRoute::Root(HomeRoot::Favorites),
            false,
            true,
            true,
        ));
        assert!(home_dashboard_should_mount(
            &HomeRoute::Root(HomeRoot::Search),
            false,
            false,
            false,
        ));
        assert!(!home_dashboard_should_mount(
            &HomeRoute::Root(HomeRoot::Search),
            false,
            true,
            true,
        ));
        assert!(!home_dashboard_should_mount(
            &HomeRoute::Root(HomeRoot::Search),
            false,
            false,
            true,
        ));
        assert!(home_dashboard_should_mount(
            &HomeRoute::Root(HomeRoot::Home),
            false,
            true,
            true,
        ));
        assert!(!home_dashboard_should_mount(
            &HomeRoute::Root(HomeRoot::Home),
            true,
            false,
            false,
        ));
    }

    #[test]
    fn only_multi_column_width_contractions_use_the_fast_resize_frame() {
        assert!(workspace_grid_contraction_is_abrupt(7, 5));
        assert!(workspace_grid_contraction_is_abrupt(7, 3));
        assert!(!workspace_grid_contraction_is_abrupt(7, 6));
        assert!(!workspace_grid_contraction_is_abrupt(7, 8));
        assert!(!workspace_grid_contraction_is_abrupt(0, 0));
    }

    #[test]
    fn abrupt_contraction_is_deferred_without_delaying_expansion() {
        assert_eq!(workspace_grid_columns_during_resize(7, 3, true), 7);
        assert_eq!(workspace_grid_columns_during_resize(7, 8, true), 8);
        assert_eq!(workspace_grid_columns_during_resize(7, 3, false), 3);
        assert_eq!(workspace_grid_columns_during_resize(0, 0, true), 1);
    }
}
