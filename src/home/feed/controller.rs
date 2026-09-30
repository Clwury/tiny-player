use std::collections::{HashMap, VecDeque};

use crate::{
    effects::{RequestScope, RequestSlot, WorkspaceIdentity},
    emby::{UserItems, VideoItemType},
    home::{
        cache::HomeSnapshot,
        model::{
            LoadState,
            library::{is_supported_view, latest_item_types},
        },
    },
};

use super::model::*;

const LATEST_CONCURRENCY: usize = 4;

/// One immutable workspace owns these scopes. Refresh invalidates every token;
/// dropped controllers reject delivery through the page's weak entity. Retired
/// blocking Latest jobs keep their concurrency permit until completion, but can
/// only release that permit: they cannot touch data, images or notifications.
#[derive(Debug)]
pub(in crate::home) struct FeedController {
    pub(in crate::home) state: FeedState,
    identity: WorkspaceIdentity,
    snapshot: RequestSlot,
    views: RequestSlot,
    resume: RequestSlot,
    latest: HashMap<String, RequestSlot>,
    latest_queue: VecDeque<String>,
    latest_in_flight: Vec<FeedRequest>,
}

impl FeedController {
    pub(in crate::home) fn new(identity: WorkspaceIdentity) -> Self {
        Self {
            state: FeedState::default(),
            snapshot: RequestSlot::new(RequestScope::HomeSnapshot, identity.clone()),
            views: RequestSlot::new(RequestScope::UserViews, identity.clone()),
            resume: RequestSlot::new(RequestScope::ResumeItems, identity.clone()),
            identity,
            latest: HashMap::new(),
            latest_queue: VecDeque::new(),
            latest_in_flight: Vec::new(),
        }
    }

    pub(in crate::home) fn dispatch(&mut self, intent: FeedIntent) -> Vec<FeedRequest> {
        match intent {
            FeedIntent::LoadSnapshot if self.state.home_snapshot.can_start() => {
                self.state.home_snapshot = LoadState::Loading;
                vec![FeedRequest {
                    kind: FeedRequestKind::Snapshot,
                    token: self.snapshot.issue(),
                }]
            }
            FeedIntent::LoadViews if self.state.views_load.can_start() => {
                self.state.views_load = LoadState::Loading;
                self.state.user_views_failed = None;
                vec![FeedRequest {
                    kind: FeedRequestKind::Views,
                    token: self.views.issue(),
                }]
            }
            FeedIntent::LoadResume if self.state.resume_load.can_start() => {
                self.state.resume_load = LoadState::Loading;
                self.state.resume_items_failed = None;
                vec![FeedRequest {
                    kind: FeedRequestKind::Resume,
                    token: self.resume.issue(),
                }]
            }
            FeedIntent::Refresh => {
                self.snapshot.invalidate();
                self.views.invalidate();
                self.resume.invalidate();
                self.latest.clear();
                self.latest_queue.clear();
                self.state = FeedState {
                    home_snapshot: LoadState::Loaded,
                    ..FeedState::default()
                };
                let mut requests = self.dispatch(FeedIntent::LoadViews);
                requests.extend(self.dispatch(FeedIntent::LoadResume));
                requests
            }
            FeedIntent::RefreshResume if !self.state.resume_load.is_loading() => {
                self.state.resume_load = LoadState::Idle;
                self.dispatch(FeedIntent::LoadResume)
            }
            FeedIntent::LoadLatest => {
                if let Some(views) = &self.state.user_views {
                    for view in &views.items {
                        if latest_item_types(view.collection_type.as_deref()).is_some()
                            && !self
                                .state
                                .user_view_items_rows
                                .get(&view.id)
                                .is_some_and(|row| row.loading)
                            && !self.latest_queue.contains(&view.id)
                        {
                            self.latest_queue.push_back(view.id.clone());
                        }
                    }
                }
                self.pump_latest()
            }
            _ => Vec::new(),
        }
    }

    pub(in crate::home) fn pump_latest(&mut self) -> Vec<FeedRequest> {
        let mut requests = Vec::new();
        while self.latest_in_flight.len() < LATEST_CONCURRENCY {
            let Some(view_id) = self.latest_queue.pop_front() else {
                break;
            };
            let Some(item_types) = self
                .state
                .user_views
                .as_ref()
                .and_then(|views| views.items.iter().find(|view| view.id == view_id))
                .and_then(|view| latest_item_types(view.collection_type.as_deref()))
            else {
                continue;
            };
            let row = self
                .state
                .user_view_items_rows
                .entry(view_id.clone())
                .or_default();
            if row.loading {
                continue;
            }
            row.loading = true;
            let token = self
                .latest
                .entry(view_id.clone())
                .or_insert_with(|| {
                    RequestSlot::new(
                        RequestScope::LatestItems {
                            view_id: view_id.clone(),
                        },
                        self.identity.clone(),
                    )
                })
                .issue();
            let request = FeedRequest {
                kind: FeedRequestKind::Latest {
                    view_id,
                    item_types,
                },
                token,
            };
            self.latest_in_flight.push(request.clone());
            requests.push(request);
        }
        requests
    }

    pub(in crate::home) fn accepts(&self, request: &FeedRequest) -> bool {
        if !request.token.is_for(&self.identity) {
            return false;
        }
        let slot = match &request.kind {
            FeedRequestKind::Snapshot => Some(&self.snapshot),
            FeedRequestKind::Views => Some(&self.views),
            FeedRequestKind::Resume => Some(&self.resume),
            FeedRequestKind::Latest { view_id, .. } => self.latest.get(view_id),
        };
        slot.is_some_and(|slot| request.token.is_current(slot))
    }

    pub(in crate::home) fn complete(
        &mut self,
        request: &FeedRequest,
        response: FeedResponse,
    ) -> FeedUpdate {
        if !matches!(
            (&request.kind, &response),
            (FeedRequestKind::Snapshot, FeedResponse::Snapshot(_))
                | (FeedRequestKind::Views, FeedResponse::Views(_))
                | (FeedRequestKind::Resume, FeedResponse::Resume(_))
                | (FeedRequestKind::Latest { .. }, FeedResponse::Latest(_))
        ) {
            return FeedUpdate::Ignored;
        }
        if matches!(request.kind, FeedRequestKind::Latest { .. }) {
            self.latest_in_flight
                .retain(|job| job.token != request.token);
        }
        if !self.accepts(request) {
            return FeedUpdate::Ignored;
        }
        let slot = match &request.kind {
            FeedRequestKind::Snapshot => &mut self.snapshot,
            FeedRequestKind::Views => &mut self.views,
            FeedRequestKind::Resume => &mut self.resume,
            FeedRequestKind::Latest { view_id, .. } => self.latest.get_mut(view_id).unwrap(),
        };
        if !slot.commit(&request.token) {
            return FeedUpdate::Ignored;
        }
        let failure = match response {
            FeedResponse::Snapshot(result) => {
                self.state.home_snapshot = if result.is_ok() {
                    LoadState::Loaded
                } else {
                    LoadState::Failed
                };
                match result {
                    Ok(mut snapshot) => {
                        if let Some(snapshot) = &mut snapshot {
                            self.hydrate_sections(snapshot);
                        }
                        return FeedUpdate::Snapshot(snapshot);
                    }
                    Err(error) => error.to_string(),
                }
            }
            FeedResponse::Views(result) => {
                self.state.views_load = if result.is_ok() {
                    LoadState::Loaded
                } else {
                    LoadState::Failed
                };
                match result {
                    Ok(mut views) => {
                        views.items.retain(is_supported_view);
                        views.total_record_count = views.items.len() as u32;
                        self.state.user_views = Some(views);
                        self.state.user_views_failed = None;
                        return FeedUpdate::Views;
                    }
                    Err(error) => {
                        let message = if self.state.user_views.is_some() {
                            format!("刷新失败：{error}")
                        } else {
                            format!("加载首页失败：{error}")
                        };
                        self.state.user_views_failed = Some(message.clone());
                        message
                    }
                }
            }
            FeedResponse::Resume(result) => {
                self.state.resume_load = if result.is_ok() {
                    LoadState::Loaded
                } else {
                    LoadState::Failed
                };
                match result {
                    Ok(mut items) => {
                        items.items.retain(|item| {
                            !item.id.trim().is_empty()
                                && matches!(item.item_type.as_deref(), Some("Movie" | "Episode"))
                        });
                        self.state.resume_items = Some(items);
                        self.state.resume_items_failed = None;
                        return FeedUpdate::Resume;
                    }
                    Err(error) => {
                        let message = if self.state.resume_items.is_some() {
                            format!("刷新失败：{error}")
                        } else {
                            format!("加载继续观看失败：{error}")
                        };
                        self.state.resume_items_failed = Some(message.clone());
                        message
                    }
                }
            }
            FeedResponse::Latest(result) => {
                let FeedRequestKind::Latest { view_id, .. } = &request.kind else {
                    unreachable!()
                };
                let allowed = self
                    .state
                    .user_views
                    .as_ref()
                    .and_then(|views| views.items.iter().find(|view| view.id == *view_id))
                    .and_then(|view| latest_item_types(view.collection_type.as_deref()))
                    .unwrap_or_default();
                let row = self
                    .state
                    .user_view_items_rows
                    .entry(view_id.clone())
                    .or_default();
                row.loading = false;
                match result {
                    Ok(mut items) => {
                        items.retain(|item| valid_latest(item, &allowed));
                        row.items = Some(UserItems {
                            total_record_count: items.len() as u32,
                            items,
                        });
                        return FeedUpdate::Latest(view_id.clone());
                    }
                    Err(error) => {
                        if row.items.is_some() {
                            format!("刷新失败：{error}")
                        } else {
                            format!("加载媒体库内容失败：{error}")
                        }
                    }
                }
            }
        };
        FeedUpdate::Failed {
            kind: request.kind.clone(),
            message: failure,
        }
    }

    pub(in crate::home) fn hydrate_sections(&mut self, snapshot: &mut HomeSnapshot) {
        if let Some(section) = snapshot.user_views.take()
            && self.state.user_views.is_none()
        {
            let mut views = section.data;
            views.items.retain(is_supported_view);
            views.total_record_count = views.items.len() as u32;
            self.state.user_views = Some(views);
            self.state.user_views_failed = None;
        }
        if let Some(section) = snapshot.resume_items.take()
            && self.state.resume_items.is_none()
        {
            let mut items = section.data;
            items.items.retain(|item| {
                !item.id.trim().is_empty()
                    && matches!(item.item_type.as_deref(), Some("Movie" | "Episode"))
            });
            self.state.resume_items = Some(items);
            self.state.resume_items_failed = None;
        }
        for (view_id, section) in std::mem::take(&mut snapshot.latest_items_by_view) {
            let row = self.state.user_view_items_rows.entry(view_id).or_default();
            if row.items.is_none() {
                let mut items = section.data;
                items.items.retain(|item| {
                    valid_latest(
                        item,
                        &[
                            VideoItemType::Movie,
                            VideoItemType::Series,
                            VideoItemType::Episode,
                        ],
                    )
                });
                items.total_record_count = items.items.len() as u32;
                row.items = Some(items);
            }
        }
    }
}

fn valid_latest(item: &crate::emby::UserItem, allowed: &[VideoItemType]) -> bool {
    !item.id.trim().is_empty()
        && allowed
            .iter()
            .any(|kind| item.item_type.as_deref() == Some(kind.as_str()))
        && (item.item_type.as_deref() != Some("Episode")
            || item
                .series_id
                .as_deref()
                .is_some_and(|id| !id.trim().is_empty()))
}
