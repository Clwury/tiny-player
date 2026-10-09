use super::HomeController;
use crate::{
    home::model::favorites::FavoriteItemType,
    home::model::navigation::{HomeRoot, HomeRoute, NavigationChange},
};

pub(in crate::home) enum NavigationIntent {
    Root(HomeRoot),
    Favorites(FavoriteItemType),
    Back,
}
impl HomeController {
    pub(in crate::home) fn route(&self) -> &HomeRoute {
        self.navigation.current()
    }
    pub(in crate::home) fn root(&self) -> HomeRoot {
        self.navigation.root()
    }
    pub(in crate::home) fn title(&self) -> &str {
        self.navigation.title()
    }
    pub(in crate::home) fn dispatch_navigation(
        &mut self,
        intent: NavigationIntent,
    ) -> NavigationChange {
        match intent {
            NavigationIntent::Root(root) => self.navigation.select_root(root),
            NavigationIntent::Favorites(kind) => self.navigation.push_favorite_items(kind),
            NavigationIntent::Back => self.navigation.pop(),
        }
    }
}

/// Opening a detail is a domain intent. The controller chooses the appropriate
/// model and rejects incomplete resume entries before the UI mounts resources.
pub(in crate::home) enum OpenDetailIntent<'a> {
    Item(&'a crate::emby::UserItem),
    Resume(&'a crate::emby::ResumeItem),
}
impl HomeController {
    pub(in crate::home) fn dispatch_open_detail(
        &mut self,
        intent: OpenDetailIntent<'_>,
        identity: crate::effects::WorkspaceIdentity,
    ) -> Result<Option<NavigationChange>, &'static str> {
        use crate::home::detail::controller::DetailController;
        let detail = match intent {
            OpenDetailIntent::Item(item) => DetailController::from_user_item(item, identity),
            OpenDetailIntent::Resume(item) => match item.item_type.as_deref() {
                Some("Movie") => DetailController::from_resume_movie(item, identity),
                Some("Episode") => Some(
                    DetailController::from_resume_episode(item, identity)
                        .ok_or("继续观看剧集缺少 SeriesId，无法打开详情")?,
                ),
                _ => None,
            },
        };
        Ok(detail.map(|detail| self.open_detail(detail)))
    }
}
