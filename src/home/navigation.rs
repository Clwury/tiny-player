use crate::emby::VideoItemType;

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
    Library {
        view_id: String,
        title: String,
        item_types: Vec<VideoItemType>,
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
            Self::Library { title, .. } => Some(title),
            Self::Detail { .. } => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HomeNavigation {
    stack: Vec<HomeRoute>,
}

impl Default for HomeNavigation {
    fn default() -> Self {
        Self {
            stack: vec![HomeRoute::Root(HomeRoot::Home)],
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

    pub(crate) fn select_root(&mut self, root: HomeRoot) -> bool {
        if self.current() == &HomeRoute::Root(root) {
            return false;
        }

        self.stack.clear();
        self.stack.push(HomeRoute::Root(root));
        true
    }

    pub(crate) fn push_library(
        &mut self,
        view_id: String,
        title: String,
        item_types: Vec<VideoItemType>,
    ) {
        self.stack.push(HomeRoute::Library {
            view_id,
            title,
            item_types,
        });
    }

    pub(crate) fn push_detail(&mut self, root_item_id: String, episode_id: Option<String>) {
        self.stack.push(HomeRoute::Detail {
            root_item_id,
            episode_id,
        });
    }

    pub(crate) fn pop(&mut self) -> bool {
        if self.stack.len() <= 1 {
            return false;
        }
        self.stack.pop();
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.stack.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_push_and_pop_to_exact_source() {
        let mut navigation = HomeNavigation::default();
        navigation.push_library("movies".into(), "电影".into(), vec![VideoItemType::Movie]);
        navigation.push_detail("movie-1".into(), None);

        assert!(matches!(navigation.current(), HomeRoute::Detail { .. }));
        assert!(navigation.pop());
        assert!(
            matches!(navigation.current(), HomeRoute::Library { view_id, .. } if view_id == "movies")
        );
        assert!(navigation.pop());
        assert_eq!(navigation.current(), &HomeRoute::Root(HomeRoot::Home));
    }

    #[test]
    fn selecting_sidebar_root_clears_child_routes() {
        let mut navigation = HomeNavigation::default();
        navigation.push_detail("movie-1".into(), None);

        assert!(navigation.select_root(HomeRoot::Search));

        assert_eq!(navigation.root(), HomeRoot::Search);
        assert_eq!(navigation.current(), &HomeRoute::Root(HomeRoot::Search));
        assert_eq!(navigation.len(), 1);
    }

    #[test]
    fn selecting_exact_active_root_is_a_no_op_but_clears_child_routes() {
        let mut navigation = HomeNavigation::default();

        assert!(!navigation.select_root(HomeRoot::Home));

        navigation.push_detail("movie-1".into(), None);

        assert!(navigation.select_root(HomeRoot::Home));
        assert_eq!(navigation.current(), &HomeRoute::Root(HomeRoot::Home));
        assert_eq!(navigation.len(), 1);
    }
}
