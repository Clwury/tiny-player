#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotificationScope {
    Home,
    Favorites,
    Search,
    Library,
    Detail,
}

#[derive(Clone, Debug)]
pub(crate) struct ActionNotification {
    pub(crate) scope: NotificationScope,
    pub(crate) key: String,
}
