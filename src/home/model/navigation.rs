use crate::{
    emby::VideoItemType,
    home::{
        detail::controller::{DetailController, DetailId},
        model::favorites::FavoriteItemType,
    },
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum HomeRoot {
    #[default]
    Home,
    Favorites,
    Search,
}

impl HomeRoot {
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Home => "首页",
            Self::Favorites => "收藏",
            Self::Search => "搜索",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HomeRoute {
    Root(HomeRoot),
    FavoriteItems {
        item_type: FavoriteItemType,
    },
    Library {
        view_id: String,
        title: String,
        item_types: Vec<VideoItemType>,
    },
    Person {
        person_id: String,
        title: String,
    },
    Genre {
        genre_key: String,
        title: String,
    },
    Detail {
        root_item_id: String,
        episode_id: Option<String>,
    },
}

impl HomeRoute {
    pub(crate) fn title(&self) -> Option<&str> {
        match self {
            Self::Root(root) => Some(root.title()),
            Self::FavoriteItems { item_type } => Some(super::favorite_section_title(*item_type)),
            Self::Library { title, .. }
            | Self::Person { title, .. }
            | Self::Genre { title, .. } => Some(title),
            Self::Detail { .. } => None,
        }
    }
}

/// Owns routes and every live detail controller. Transitions deactivate hidden
/// entries immediately and return retired models for adapter-controlled release.
#[derive(Debug)]
pub(crate) struct HomeNavigation {
    stack: Vec<HomeRoute>,
    detail: Option<DetailController>,
    history: Vec<DetailController>,
}

#[derive(Debug, Default)]
pub(crate) struct NavigationChange {
    pub(crate) changed: bool,
    pub(crate) hidden: Option<DetailId>,
    pub(crate) removed: Vec<DetailController>,
}

impl Default for HomeNavigation {
    fn default() -> Self {
        Self {
            stack: vec![HomeRoute::Root(HomeRoot::Home)],
            detail: None,
            history: Vec::new(),
        }
    }
}

impl HomeNavigation {
    pub(crate) fn root(&self) -> HomeRoot {
        match self.stack.first() {
            Some(HomeRoute::Root(root)) => *root,
            _ => HomeRoot::Home,
        }
    }

    pub(crate) fn current(&self) -> &HomeRoute {
        self.stack
            .last()
            .expect("Home route stack always contains its root route")
    }

    pub(crate) fn title(&self) -> &str {
        match self.current() {
            HomeRoute::Detail { .. } => self
                .detail()
                .map(|detail| detail.view_model().title.as_str())
                .unwrap_or_else(|| self.root().title()),
            route => route.title().unwrap_or_else(|| self.root().title()),
        }
    }

    pub(crate) fn detail(&self) -> Option<&DetailController> {
        self.detail.as_ref()
    }
    pub(crate) fn detail_mut(&mut self) -> Option<&mut DetailController> {
        self.detail.as_mut()
    }
    /// Cross-feature mutations may update media data, but cannot modify the
    /// history structure, controller identities or activation slots.
    pub(crate) fn detail_models_mut(
        &mut self,
    ) -> (
        Option<&mut crate::home::detail::model::SeriesDetailModel>,
        impl Iterator<Item = &mut crate::home::detail::model::SeriesDetailModel>,
    ) {
        (
            self.detail.as_mut().map(|detail| &mut detail.state),
            self.history.iter_mut().map(|detail| &mut detail.state),
        )
    }

    fn retire_details(&mut self) -> Vec<DetailController> {
        let mut retired: Vec<_> = self
            .detail
            .take()
            .into_iter()
            .chain(std::mem::take(&mut self.history))
            .collect();
        for detail in &mut retired {
            detail.deactivate();
        }
        retired
    }

    pub(crate) fn select_root(&mut self, root: HomeRoot) -> NavigationChange {
        if self.current() == &HomeRoute::Root(root) {
            return NavigationChange::default();
        }
        self.stack.clear();
        self.stack.push(HomeRoute::Root(root));
        NavigationChange {
            changed: true,
            removed: self.retire_details(),
            ..Default::default()
        }
    }

    pub(crate) fn push_library(
        &mut self,
        view_id: String,
        title: String,
        item_types: Vec<VideoItemType>,
    ) -> NavigationChange {
        self.stack.push(HomeRoute::Library {
            view_id,
            title,
            item_types,
        });
        NavigationChange {
            changed: true,
            removed: self.retire_details(),
            ..Default::default()
        }
    }

    pub(crate) fn open_detail(
        &mut self,
        mut detail: DetailController,
        versions: &std::collections::HashMap<String, crate::home::video_version::VideoVersion>,
    ) -> NavigationChange {
        detail.state.resume_video_version = detail
            .view_model()
            .resume_media_item_id()
            .and_then(|id| versions.get(id))
            .cloned();
        let mut change = NavigationChange {
            changed: true,
            ..Default::default()
        };
        change.hidden = self.hide_detail();
        self.stack.push(HomeRoute::Detail {
            root_item_id: detail.view_model().series_id.clone(),
            episode_id: detail.view_model().preferred_episode_id.clone(),
        });
        self.detail = Some(detail);
        change
    }

    fn hide_detail(&mut self) -> Option<DetailId> {
        let mut current = self.detail.take()?;
        current.deactivate();
        let id = current.id();
        self.history.push(current);
        Some(id)
    }

    pub(crate) fn push_person(&mut self, person_id: String, title: String) -> NavigationChange {
        let hidden = self.hide_detail();
        self.stack.push(HomeRoute::Person { person_id, title });
        NavigationChange {
            changed: true,
            hidden,
            ..Default::default()
        }
    }

    pub(crate) fn push_favorite_items(&mut self, item_type: FavoriteItemType) -> NavigationChange {
        self.stack.push(HomeRoute::FavoriteItems { item_type });
        NavigationChange {
            changed: true,
            removed: self.retire_details(),
            ..Default::default()
        }
    }

    pub(crate) fn push_genre(&mut self, genre_key: String, title: String) -> NavigationChange {
        let hidden = self.hide_detail();
        self.stack.push(HomeRoute::Genre { genre_key, title });
        NavigationChange {
            changed: true,
            hidden,
            ..Default::default()
        }
    }

    pub(crate) fn pop(&mut self) -> NavigationChange {
        if self.stack.len() <= 1 {
            return NavigationChange::default();
        }
        self.stack.pop();
        let mut removed = Vec::new();
        if let Some(mut current) = self.detail.take() {
            current.deactivate();
            removed.push(current);
        }
        if matches!(self.current(), HomeRoute::Detail { .. }) {
            self.detail = self.history.pop();
            if let Some(detail) = self.detail.as_mut() {
                detail.activate();
            }
        } else if !matches!(
            self.current(),
            HomeRoute::Person { .. } | HomeRoute::Genre { .. }
        ) {
            removed.extend(self.retire_details());
        }
        NavigationChange {
            changed: true,
            removed,
            ..Default::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn set_detail_fixture(&mut self, detail: Option<DetailController>) {
        self.detail = detail;
    }
    #[cfg(test)]
    pub(crate) fn push_detail_route_fixture(
        &mut self,
        root_item_id: String,
        episode_id: Option<String>,
    ) {
        self.stack.push(HomeRoute::Detail {
            root_item_id,
            episode_id,
        });
    }
    #[cfg(test)]
    pub(crate) fn history(&self) -> &[DetailController] {
        &self.history
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.stack.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> DetailController {
        let item = serde_json::from_value(serde_json::json!({"Id":id, "Name":id, "Type":"Series"}))
            .unwrap();
        DetailController::from_user_item(&item, Default::default()).unwrap()
    }

    #[test]
    fn same_item_visits_own_independent_state_and_back_reactivates_the_original() {
        use crate::effects::{DetailResource, WorkspaceIdentity};
        use crate::home::{
            detail::controller::DetailIntent,
            model::user_data::{PendingUserData, UserDataState},
        };
        let mut navigation = HomeNavigation::default();
        let mut first = entry("same");
        first
            .dispatch(DetailIntent::Season("season-2".into()))
            .unwrap();
        let first_id = first.id();
        let old_activation = first.activation().cloned();
        let old_request = first
            .begin(DetailResource::Item, WorkspaceIdentity::default(), 0)
            .unwrap();
        navigation.open_detail(first, &Default::default());
        let second = entry("same");
        let second_id = second.id();
        assert_ne!(first_id, second_id);
        let change = navigation.open_detail(second, &Default::default());
        assert_eq!(change.hidden, Some(first_id));
        assert!(change.removed.is_empty());
        assert!(navigation.history[0].activation().is_none());
        assert_eq!(navigation.detail().unwrap().id(), second_id);
        assert!(
            navigation
                .detail()
                .unwrap()
                .view_model()
                .selected_season_id
                .is_none()
        );
        let change = navigation.pop();
        assert_eq!(change.removed.len(), 1);
        assert_eq!(change.removed[0].id(), second_id);
        assert!(change.removed[0].activation().is_none());
        let restored = navigation.detail_mut().unwrap();
        assert_eq!(restored.id(), first_id);
        assert_ne!(restored.activation(), old_activation.as_ref());
        assert_eq!(
            restored.view_model().selected_season_id.as_deref(),
            Some("season-2")
        );
        let mut data = UserDataState::default();
        assert!(
            restored
                .complete(
                    &old_request,
                    Err(anyhow::anyhow!("hidden response")),
                    &WorkspaceIdentity::default(),
                    &mut data,
                    PendingUserData {
                        played: false,
                        favorites: &std::collections::HashSet::new()
                    }
                )
                .is_none()
        );
        assert!(
            restored
                .begin(DetailResource::Item, WorkspaceIdentity::default(), 0)
                .is_some()
        );
    }

    #[test]
    fn switching_root_retires_all_models_before_the_adapter_releases_them() {
        let mut navigation = HomeNavigation::default();
        navigation.open_detail(entry("first"), &Default::default());
        navigation.open_detail(entry("second"), &Default::default());
        let change = navigation.select_root(HomeRoot::Search);
        assert!(change.changed);
        assert_eq!(change.removed.len(), 2);
        assert!(
            change
                .removed
                .iter()
                .all(|detail| detail.activation().is_none())
        );
        assert!(navigation.detail().is_none() && navigation.history.is_empty());
        assert_eq!(navigation.current(), &HomeRoute::Root(HomeRoot::Search));
        assert_eq!(navigation.title(), "搜索");
        assert!(!navigation.select_root(HomeRoot::Search).changed);
        assert!(!navigation.pop().changed);
        // Retired payload remains available to the two-frame release adapter.
        assert_eq!(change.removed[0].view_model().series_id, "second");
        assert_eq!(change.removed[1].view_model().series_id, "first");
    }

    #[test]
    fn detail_return_preserves_library_and_favorite_routes_and_titles() {
        let mut navigation = HomeNavigation::default();
        navigation.push_library(
            "library".into(),
            "Library title".into(),
            vec![VideoItemType::Series],
        );
        navigation.open_detail(entry("series"), &Default::default());
        navigation.detail_mut().unwrap().state.title = "Loaded title".into();
        assert_eq!(navigation.title(), "Loaded title");
        let returned = navigation.pop();
        assert_eq!(returned.removed.len(), 1);
        assert_eq!(navigation.title(), "Library title");
        assert!(navigation.detail().is_none());
        navigation.select_root(HomeRoot::Favorites);
        navigation.push_favorite_items(VideoItemType::Movie.into());
        navigation.open_detail(entry("series"), &Default::default());
        assert_eq!(navigation.pop().removed.len(), 1);
        assert_eq!(
            navigation.current(),
            &HomeRoute::FavoriteItems {
                item_type: VideoItemType::Movie.into()
            }
        );
        assert_eq!(navigation.title(), "电影");
        assert!(navigation.pop().changed);
        assert_eq!(navigation.current(), &HomeRoute::Root(HomeRoot::Favorites));
    }

    #[test]
    fn opening_resume_detail_hydrates_the_played_version_in_the_navigation_owner() {
        let resume = serde_json::from_value(serde_json::json!({"Id":"episode", "Name":"Episode", "Type":"Episode", "SeriesId":"series", "ParentId":"season"})).unwrap();
        let detail = DetailController::from_resume_episode(&resume, Default::default()).unwrap();
        let mut navigation = HomeNavigation::default();
        let versions = std::collections::HashMap::from([(
            "episode".into(),
            crate::home::video_version::VideoVersion {
                source_id: "chosen".into(),
                name: Some("2160p".into()),
                ..Default::default()
            },
        )]);
        navigation.open_detail(detail, &versions);
        let detail = navigation.detail().unwrap().view_model();
        assert_eq!(
            detail.resume_video_version.as_ref().unwrap().source_id,
            "chosen"
        );
        assert_eq!(detail.selected_episode_id.as_deref(), Some("episode"));
    }

    #[test]
    fn person_routes_preserve_each_source_detail_through_nested_filmographies() {
        use crate::home::detail::controller::DetailIntent;
        let mut navigation = HomeNavigation::default();
        navigation.push_library(
            "library".into(),
            "Library".into(),
            vec![VideoItemType::Series],
        );
        let mut first = entry("first");
        first
            .dispatch(DetailIntent::Season("season-2".into()))
            .unwrap();
        let first_id = first.id();
        navigation.open_detail(first, &Default::default());
        let change = navigation.push_person("person-1".into(), "First person".into());
        assert_eq!(change.hidden, Some(first_id));
        assert!(change.removed.is_empty());
        assert!(navigation.detail().is_none());
        assert_eq!(navigation.title(), "First person");
        let second = entry("second");
        let second_id = second.id();
        navigation.open_detail(second, &Default::default());
        navigation.push_person("person-2".into(), "Second person".into());
        navigation.open_detail(entry("third"), &Default::default());
        let change = navigation.pop();
        assert_eq!(change.removed.len(), 1);
        assert_eq!(navigation.title(), "Second person");
        assert_eq!(navigation.history.len(), 2);
        assert!(navigation.pop().removed.is_empty());
        assert_eq!(navigation.detail().unwrap().id(), second_id);
        assert!(navigation.detail().unwrap().activation().is_some());
        let change = navigation.pop();
        assert_eq!(change.removed[0].id(), second_id);
        assert_eq!(navigation.title(), "First person");
        assert_eq!(navigation.history.len(), 1);
        assert!(navigation.pop().removed.is_empty());
        let restored = navigation.detail().unwrap();
        assert_eq!(restored.id(), first_id);
        assert_eq!(
            restored.view_model().selected_season_id.as_deref(),
            Some("season-2")
        );
        assert!(restored.activation().is_some());
        assert_eq!(navigation.pop().removed[0].id(), first_id);
        assert_eq!(navigation.title(), "Library");
    }

    #[test]
    fn switching_root_from_person_retires_all_hidden_details() {
        let mut navigation = HomeNavigation::default();
        navigation.open_detail(entry("first"), &Default::default());
        navigation.push_person("person".into(), "Person".into());
        navigation.open_detail(entry("second"), &Default::default());
        navigation.push_person("other".into(), "Other".into());
        let change = navigation.select_root(HomeRoot::Home);
        assert_eq!(change.removed.len(), 2);
        assert!(navigation.history.is_empty());
        assert!(navigation.detail().is_none());
        assert_eq!(navigation.current(), &HomeRoute::Root(HomeRoot::Home));
    }

    #[test]
    fn repeated_back_keeps_the_selected_root() {
        let mut navigation = HomeNavigation::default();
        navigation.select_root(HomeRoot::Favorites);
        navigation.push_favorite_items(VideoItemType::Movie.into());
        navigation.push_detail_route_fixture("movie".into(), None);
        assert!(navigation.pop().changed);
        assert!(navigation.pop().changed);
        assert!(!navigation.pop().changed);
        assert!(!navigation.pop().changed);
        assert_eq!(navigation.current(), &HomeRoute::Root(HomeRoot::Favorites));
    }

    #[test]
    fn routes_push_and_pop_to_exact_source() {
        let mut navigation = HomeNavigation::default();
        navigation.push_library("movies".into(), "电影".into(), vec![VideoItemType::Movie]);
        navigation.push_detail_route_fixture("movie-1".into(), None);

        assert!(matches!(navigation.current(), HomeRoute::Detail { .. }));
        assert!(navigation.pop().changed);
        assert!(
            matches!(navigation.current(), HomeRoute::Library { view_id, .. } if view_id == "movies")
        );
        assert!(navigation.pop().changed);
        assert_eq!(navigation.current(), &HomeRoute::Root(HomeRoot::Home));
    }

    #[test]
    fn selecting_sidebar_root_clears_child_routes() {
        let mut navigation = HomeNavigation::default();
        navigation.push_detail_route_fixture("movie-1".into(), None);

        assert!(navigation.select_root(HomeRoot::Search).changed);

        assert_eq!(navigation.root(), HomeRoot::Search);
        assert_eq!(navigation.current(), &HomeRoute::Root(HomeRoot::Search));
        assert_eq!(navigation.len(), 1);
    }

    #[test]
    fn selecting_exact_active_root_is_a_no_op_but_clears_child_routes() {
        let mut navigation = HomeNavigation::default();

        assert!(!navigation.select_root(HomeRoot::Home).changed);

        navigation.push_detail_route_fixture("movie-1".into(), None);

        assert!(navigation.select_root(HomeRoot::Home).changed);
        assert_eq!(navigation.current(), &HomeRoute::Root(HomeRoot::Home));
        assert_eq!(navigation.len(), 1);
    }
}
