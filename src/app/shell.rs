//! Root route and mounted feature lifetime. Generic handles keep the controller
//! independent of GPUI; the runner supplies entities and subscription handles.
use crate::effects::{EffectHandle, RequestScope, RequestSlot, RequestToken, WorkspaceIdentity};

#[derive(Clone, Debug)]
pub(super) enum MountedPage<H, P> {
    Servers,
    Home(H),
    Playback { page: P, return_to: H },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum AppRoute<'a> {
    Servers,
    Home(&'a WorkspaceIdentity),
    Playback(&'a WorkspaceIdentity),
}

struct Mount<S> {
    identity: WorkspaceIdentity,
    request: RequestSlot,
    subscription: EffectHandle<S>,
}
impl<S> Mount<S> {
    fn new(scope: RequestScope, identity: WorkspaceIdentity) -> (Self, RequestToken) {
        let mut request = RequestSlot::new(scope, identity.clone());
        let token = request.issue();
        (
            Self {
                identity,
                request,
                subscription: EffectHandle::default(),
            },
            token,
        )
    }
    fn accepts(&self, token: &RequestToken) -> bool {
        token.is_for(&self.identity) && token.is_current(&self.request)
    }
    fn subscribe(&mut self, token: &RequestToken, subscription: S) {
        if self.accepts(token) {
            self.subscription.replace(subscription);
        }
    }
}

// The shell owns route, opaque mounted entities and their subscription scopes.
// Mount/replace/back are the only writes. Home survives playback replacement;
// servers/home replacement invalidates all old scopes before releasing entities.
pub(super) struct ShellController<H, P, S> {
    home: Option<Mount<S>>,
    playback: Option<Mount<S>>,
    page: MountedPage<H, P>,
}

impl<H, P, S> Default for ShellController<H, P, S> {
    fn default() -> Self {
        Self {
            home: None,
            playback: None,
            page: MountedPage::Servers,
        }
    }
}

impl<H: Clone, P, S> ShellController<H, P, S> {
    pub(super) fn page(&self) -> &MountedPage<H, P> {
        &self.page
    }

    pub(super) fn route(&self) -> AppRoute<'_> {
        match &self.page {
            MountedPage::Servers => AppRoute::Servers,
            MountedPage::Home(_) => {
                AppRoute::Home(&self.home.as_ref().expect("mounted Home scope").identity)
            }
            MountedPage::Playback { .. } => AppRoute::Playback(
                &self
                    .playback
                    .as_ref()
                    .expect("mounted Playback scope")
                    .identity,
            ),
        }
    }

    pub(super) fn home(&self) -> Option<&H> {
        match &self.page {
            MountedPage::Home(home)
            | MountedPage::Playback {
                return_to: home, ..
            } => Some(home),
            MountedPage::Servers => None,
        }
    }

    pub(super) fn show_servers(&mut self) {
        self.playback = None;
        self.home = None;
        self.page = MountedPage::Servers;
    }

    pub(super) fn mount_home(&mut self, home: H, identity: WorkspaceIdentity) -> RequestToken {
        let (mount, token) = Mount::new(RequestScope::AppHomeMount, identity);
        self.playback = None;
        self.home = Some(mount);
        self.page = MountedPage::Home(home);
        token
    }

    pub(super) fn mount_playback(&mut self, page: P) -> Option<RequestToken> {
        let return_to = self.home()?.clone();
        let identity = self.home.as_ref()?.identity.clone();
        let (mount, token) = Mount::new(RequestScope::AppPlaybackMount, identity);
        self.playback = Some(mount);
        self.page = MountedPage::Playback { page, return_to };
        Some(token)
    }

    pub(super) fn return_home(&mut self) -> bool {
        let MountedPage::Playback { return_to, .. } = &self.page else {
            return false;
        };
        let return_to = return_to.clone();
        self.playback = None;
        self.page = MountedPage::Home(return_to);
        true
    }

    pub(super) fn accepts_home(&self, token: &RequestToken) -> bool {
        self.home.as_ref().is_some_and(|mount| mount.accepts(token))
    }
    pub(super) fn accepts_playback(&self, token: &RequestToken) -> bool {
        self.playback
            .as_ref()
            .is_some_and(|mount| mount.accepts(token))
    }
    pub(super) fn subscribe_home(&mut self, token: &RequestToken, subscription: S) {
        if let Some(mount) = &mut self.home {
            mount.subscribe(token, subscription);
        }
    }
    pub(super) fn subscribe_playback(&mut self, token: &RequestToken, subscription: S) {
        if let Some(mount) = &mut self.playback {
            mount.subscribe(token, subscription);
        }
    }

    #[cfg(test)]
    pub(super) fn home_token(&self) -> Option<RequestToken> {
        self.home.as_ref()?.request.latest()
    }
    #[cfg(test)]
    pub(super) fn playback_token(&self) -> Option<RequestToken> {
        self.playback.as_ref()?.request.latest()
    }
}

#[cfg(test)]
mod tests;
