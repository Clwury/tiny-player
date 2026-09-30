#[cfg(test)]
use super::model::library::latest_item_types;
#[cfg(test)]
use super::model::library::library_item_types;
pub(crate) mod controller;
mod effect;
use super::adapter::EmbyHomeGateway;
#[cfg(test)]
use super::paged_items::PAGED_ITEMS_LIMIT;
use super::{
    HomeContent, HomeContentEvent,
    navigation::HomeRoute,
    notification::{
        NotificationScope, library_initial_notification_key, library_load_more_notification_key,
        library_refresh_notification_key,
    },
};
use crate::effects::EffectHandle;
use crate::emby::{SortOrder, UserItems, UserItemsSort, UserView};
#[cfg(test)]
use crate::{effects::WorkspaceIdentity, emby::VideoItemType};
#[cfg(test)]
use controller::LibraryController;
pub(crate) use controller::available_library_sorts;
use controller::{LibraryFailure, LibraryIntent, LibraryRequest, LibraryTransition};
use gpui::{AppContext as _, Context};

/// View resources live with HomeContent; business state lives in HomeController.
#[derive(Debug, Default)]
pub(super) struct LibraryResources {
    pub(super) effect: EffectHandle<gpui::Task<()>>,
    pub(super) presentation: LibraryPresentation,
}

#[derive(Debug, Default)]
pub(super) struct LibraryPresentation {
    pub(super) sort_menu_open: bool,
    pub(super) grid: super::presentation::GridPresentation,
}

pub(super) struct LibraryView<'a> {
    pub(super) model: controller::LibraryVm<'a>,
    pub(super) presentation: &'a LibraryPresentation,
}

impl HomeContent {
    pub(super) fn open_library_for_view(&mut self, view: &UserView, cx: &mut Context<Self>) {
        let Some((change, transition)) = self.controller.open_library(view) else {
            return;
        };
        self.item_context_menu = None;
        self.library_resources.entry(view.id.clone()).or_default();
        super::detail::state::apply_navigation_change(change, &mut self.detail_resources);
        self.clear_library_notifications(&view.id);
        self.apply_library_transition(&view.id, transition, cx);
        cx.emit(HomeContentEvent::TitleChanged);
        cx.notify();
    }

    pub(super) fn open_library_by_id(&mut self, view_id: &str, cx: &mut Context<Self>) {
        let view = self.controller.user_view(view_id).cloned();
        if let Some(view) = view {
            self.open_library_for_view(&view, cx);
        }
    }

    pub(super) fn toggle_current_library_sort_menu(&mut self, cx: &mut Context<Self>) {
        let HomeRoute::Library { view_id, .. } = self.controller.route() else {
            return;
        };
        let Some(state) = self.library_resources.get_mut(view_id) else {
            return;
        };

        state.presentation.sort_menu_open = !state.presentation.sort_menu_open;
        cx.notify();
    }

    pub(super) fn close_current_library_sort_menu(&mut self, cx: &mut Context<Self>) {
        let HomeRoute::Library { view_id, .. } = self.controller.route() else {
            return;
        };
        let Some(state) = self.library_resources.get_mut(view_id) else {
            return;
        };
        if state.presentation.sort_menu_open {
            state.presentation.sort_menu_open = false;
            cx.notify();
        }
    }

    pub(super) fn select_library_sort_by(
        &mut self,
        view_id: String,
        sort_by: UserItemsSort,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_library(&view_id, LibraryIntent::SortBy(sort_by), cx);
    }
    pub(super) fn select_library_sort_order(
        &mut self,
        view_id: String,
        sort_order: SortOrder,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_library(&view_id, LibraryIntent::SortOrder(sort_order), cx);
    }
    pub(super) fn auto_load_more_library(&mut self, view_id: &str, cx: &mut Context<Self>) {
        if !matches!(self.controller.route(), HomeRoute::Library {view_id: current, ..} if current == view_id)
        {
            return;
        }
        self.dispatch_library(view_id, LibraryIntent::LoadMore { automatic: true }, cx);
    }
    fn dispatch_library(&mut self, view_id: &str, intent: LibraryIntent, cx: &mut Context<Self>) {
        let Some(transition) = self.controller.dispatch_library(view_id, intent) else {
            return;
        };
        self.apply_library_transition(view_id, transition, cx);
    }
    fn apply_library_transition(
        &mut self,
        view_id: &str,
        transition: LibraryTransition,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.library_resources.get_mut(view_id) else {
            return;
        };
        if transition.close_menu {
            state.presentation.sort_menu_open = false;
        }
        if transition.cancel {
            state.effect.cancel();
        }
        if let Some(request) = transition.request {
            if request.initial {
                self.clear_library_notifications(view_id);
            } else {
                self.clear_notification(
                    NotificationScope::Library,
                    &library_load_more_notification_key(view_id),
                );
            }
            let gateway = EmbyHomeGateway {
                server: self.current_server.clone(),
                client: self.emby_client.clone(),
            };
            let task_request = request.clone();
            let task =
                cx.background_spawn(async move { effect::run_library(&gateway, &task_request) });
            let handle = cx.spawn(async move |page, cx| {
                let result = task.await;
                page.update(cx, |page, cx| page.finish_library(request, result, cx))
                    .ok();
            });
            if let Some(state) = self.library_resources.get_mut(view_id) {
                state.effect.replace(handle);
            }
        }
        if transition.notify {
            cx.notify();
        }
    }
    fn finish_library(
        &mut self,
        request: LibraryRequest,
        result: anyhow::Result<UserItems>,
        cx: &mut Context<Self>,
    ) {
        let identity = self.request_identity();
        let Some(update) = self
            .controller
            .complete_library(&request, result, &identity)
        else {
            return;
        };
        if let Some(items) = update.images {
            self.layout.content_changed();
            self.ensure_user_items_images(&items, cx);
        }
        if let Some(failure) = update.failure {
            let (key, message) = match failure {
                LibraryFailure::Initial(error) => (
                    library_initial_notification_key(&request.view_id),
                    format!("加载媒体库失败：{error}"),
                ),
                LibraryFailure::Refresh(error) => {
                    (library_refresh_notification_key(&request.view_id), error)
                }
                LibraryFailure::More(error) => (
                    library_load_more_notification_key(&request.view_id),
                    format!("加载更多媒体库内容失败：{error}"),
                ),
            };
            self.push_error_notification(NotificationScope::Library, key, message, cx);
        } else if request.initial {
            self.clear_library_notifications(&request.view_id);
        } else {
            self.clear_notification(
                NotificationScope::Library,
                &library_load_more_notification_key(&request.view_id),
            );
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn reopening_loaded_library_reuses_scroll_and_sort_selection_closes_only_its_menu(
        cx: &mut gpui::TestAppContext,
    ) {
        let server = serde_json::from_value(serde_json::json!({
            "id":"local", "server_id":"remote", "user_id":"user",
            "endpoint":{"protocol":"Https", "address":"example.com", "port":443, "path":""},
            "username":"test", "password":"", "added_at_unix":0
        }))
        .unwrap();
        let page = cx.new(|cx| {
            HomeContent::new(
                server,
                crate::emby::EmbyClient::new("test".into()).unwrap(),
                cx,
            )
        });
        let view: UserView = serde_json::from_value(serde_json::json!({
            "Id":"library", "Name":"Movies", "CollectionType":"movies"
        }))
        .unwrap();
        page.update(cx, |page, cx| {
            let mut library = LibraryController::new(
                vec![VideoItemType::Movie],
                view.id.clone(),
                page.request_identity(),
            );
            library.test_paged_mut().initial = super::super::LoadState::Loaded;
            page.controller
                .test_state_mut()
                .libraries
                .insert(view.id.clone(), library);
            page.open_library_for_view(&view, cx);
            let offset = gpui::point(gpui::px(0.0), gpui::px(-600.0));
            page.library_resources[&view.id]
                .presentation
                .grid
                .scroll_handle
                .set_offset(offset);
            page.toggle_current_library_sort_menu(cx);
            assert!(page.library_resources[&view.id].presentation.sort_menu_open);
            page.select_library_sort_by(view.id.clone(), UserItemsSort::SortName, cx);
            assert!(!page.library_resources[&view.id].presentation.sort_menu_open);
            assert_eq!(
                page.library_resources[&view.id]
                    .presentation
                    .grid
                    .scroll_handle
                    .offset(),
                offset
            );
            page.open_library_for_view(&view, cx);
            assert_eq!(page.library_resources.len(), 1);
            assert_eq!(
                page.library_resources[&view.id]
                    .presentation
                    .grid
                    .scroll_handle
                    .offset(),
                offset
            );
            assert_eq!(
                page.controller.test_state().libraries[&view.id]
                    .view_model()
                    .paged
                    .initial,
                super::super::LoadState::Loaded
            );
        });
        cx.run_until_parked();
    }

    #[test]
    fn maps_collection_types_to_v1_root_and_latest_types() {
        assert_eq!(
            library_item_types(Some("movies")),
            Some(vec![VideoItemType::Movie])
        );
        assert_eq!(
            library_item_types(Some("tvshows")),
            Some(vec![VideoItemType::Series])
        );
        assert_eq!(
            latest_item_types(Some("tvshows")),
            Some(vec![VideoItemType::Series, VideoItemType::Episode])
        );
        assert_eq!(
            library_item_types(Some(" mixed ")),
            Some(vec![VideoItemType::Movie, VideoItemType::Series])
        );
        assert_eq!(
            latest_item_types(None),
            Some(vec![
                VideoItemType::Movie,
                VideoItemType::Series,
                VideoItemType::Episode,
            ])
        );
        assert_eq!(
            library_item_types(Some("TVSHOWS")),
            Some(vec![VideoItemType::Series])
        );
        assert!(library_item_types(Some("music")).is_none());
        assert_eq!(
            library_item_types(Some("future-video-kind")),
            Some(vec![VideoItemType::Movie, VideoItemType::Series])
        );
    }

    #[test]
    fn last_content_added_sort_is_available_only_for_series_libraries() {
        let series_sorts = available_library_sorts(&[VideoItemType::Series]).collect::<Vec<_>>();
        assert!(series_sorts.contains(&UserItemsSort::DateLastContentAdded));

        let movie_sorts = available_library_sorts(&[VideoItemType::Movie]).collect::<Vec<_>>();
        assert!(!movie_sorts.contains(&UserItemsSort::DateLastContentAdded));

        let mixed_sorts = available_library_sorts(&[VideoItemType::Movie, VideoItemType::Series])
            .collect::<Vec<_>>();
        assert!(!mixed_sorts.contains(&UserItemsSort::DateLastContentAdded));
    }

    #[test]
    fn library_query_uses_selected_sort_for_every_page() {
        let mut state = LibraryController::new(
            vec![VideoItemType::Series],
            "view-1".into(),
            WorkspaceIdentity::default(),
        );
        state.dispatch(LibraryIntent::SortBy(UserItemsSort::CriticRating), 0);
        let changed = state.dispatch(LibraryIntent::SortOrder(SortOrder::Descending), 0);
        let initial = changed.request.unwrap();
        state
            .complete(
                &initial,
                Ok(UserItems {
                    items: (0..PAGED_ITEMS_LIMIT)
                        .map(|index| {
                            serde_json::from_value(serde_json::json!({
                                "Id": index.to_string(), "Name": "Series", "Type": "Series"
                            }))
                            .unwrap()
                        })
                        .collect(),
                    total_record_count: PAGED_ITEMS_LIMIT * 3,
                }),
                &WorkspaceIdentity::default(),
            )
            .unwrap();
        let more = state
            .dispatch(LibraryIntent::LoadMore { automatic: true }, 1)
            .request
            .unwrap();
        for (request, start_index) in [(&initial, 0), (&more, PAGED_ITEMS_LIMIT)] {
            let query = &request.query;
            assert_eq!(query.parent_id.as_deref(), Some("view-1"));
            assert_eq!(query.include_item_types, vec![VideoItemType::Series]);
            assert!(query.recursive);
            assert_eq!(query.start_index, start_index);
            assert_eq!(query.limit, PAGED_ITEMS_LIMIT);
            assert_eq!(query.sort_by, Some(UserItemsSort::CriticRating));
            assert_eq!(query.sort_order, SortOrder::Descending);
        }
    }

    #[test]
    fn changing_library_sort_invalidates_pages_and_closes_menu() {
        let mut state = LibraryController::new(
            vec![VideoItemType::Movie],
            "view".into(),
            WorkspaceIdentity::default(),
        );
        let pending = state.test_paged_mut().begin_initial(true).unwrap();

        let transition = state.dispatch(LibraryIntent::SortBy(UserItemsSort::DateCreated), 0);
        assert!(transition.close_menu);
        assert!(transition.request.is_some());
        state.dispatch(LibraryIntent::SortOrder(SortOrder::Descending), 0);
        assert_eq!(state.view_model().sort_by, UserItemsSort::DateCreated);
        assert_eq!(state.view_model().sort_order, SortOrder::Descending);
        assert!(state.view_model().paged.dirty);
        assert!(!state.view_model().paged.accepts_initial(&pending));
    }

    #[test]
    fn selecting_current_library_sort_only_closes_menu() {
        let mut state = LibraryController::new(
            vec![VideoItemType::Movie],
            "view".into(),
            WorkspaceIdentity::default(),
        );

        let transition = state.dispatch(LibraryIntent::SortBy(UserItemsSort::SortName), 0);
        assert!(transition.close_menu);
        assert!(transition.request.is_none());
        assert!(!state.view_model().paged.dirty);
        assert_eq!(
            state.view_model().paged.initial,
            super::super::LoadState::Idle
        );
    }
}
