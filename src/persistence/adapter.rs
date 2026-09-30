use anyhow::Result;

use crate::{
    home::cache::{self, HomeSnapshot},
    server::CachedServer,
    storage::{self, ServerCache},
};

/// File boundaries return domain snapshots only. Atomic replacement, versions,
/// account validation and private permissions remain in the existing codecs.
pub(crate) trait AppPersistence: Send + Sync {
    fn load_settings(&self) -> Result<ServerCache>;
    fn save_settings(&self, snapshot: &ServerCache) -> Result<()>;
    fn load_home(&self, server: &CachedServer) -> Result<Option<HomeSnapshot>>;
    fn save_home(&self, server: &CachedServer, snapshot: &HomeSnapshot) -> Result<()>;
}

#[derive(Default)]
pub(crate) struct FilePersistence {
    #[cfg(test)]
    pub(crate) settings_path: Option<std::path::PathBuf>,
    #[cfg(test)]
    pub(crate) home_path: Option<std::path::PathBuf>,
}

impl AppPersistence for FilePersistence {
    fn load_settings(&self) -> Result<ServerCache> {
        #[cfg(test)]
        if let Some(path) = &self.settings_path {
            return storage::load_or_init_from(path);
        }
        storage::load_or_init()
    }

    fn save_settings(&self, snapshot: &ServerCache) -> Result<()> {
        #[cfg(test)]
        if let Some(path) = &self.settings_path {
            return storage::save_to(snapshot, path);
        }
        storage::save(snapshot)
    }

    fn load_home(&self, server: &CachedServer) -> Result<Option<HomeSnapshot>> {
        #[cfg(test)]
        if let Some(path) = &self.home_path {
            return cache::load_snapshot_from(path, server);
        }
        cache::load_snapshot(server)
    }

    fn save_home(&self, server: &CachedServer, snapshot: &HomeSnapshot) -> Result<()> {
        #[cfg(test)]
        if let Some(path) = &self.home_path {
            return cache::save_snapshot_to(path, snapshot);
        }
        cache::save_snapshot(server, snapshot)
    }
}
