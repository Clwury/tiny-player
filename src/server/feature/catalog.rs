use crate::server::CachedServer;
use anyhow::{Context, Result};

/// Server feature owns the sole mutable catalog. Commands produce candidate
/// copies for transactional saving; only successful persistence commits them.
#[derive(Clone, Debug, Default)]
pub(crate) struct ServerCatalog {
    pub(crate) servers: Vec<CachedServer>,
    pub(crate) auto_start_server_id: Option<String>,
}

pub fn upsert_server(cache: &mut ServerCatalog, mut server: CachedServer) -> String {
    deduplicate_servers(cache);
    if let Some(existing) = cache
        .servers
        .iter_mut()
        .find(|existing| same_server(existing, &server))
    {
        server.id = existing.id.clone();
        *existing = server;
        existing.id.clone()
    } else {
        let id = server.id.clone();
        cache.servers.push(server);
        id
    }
}

pub fn update_server_by_id(cache: &mut ServerCatalog, server: CachedServer) -> Result<()> {
    let existing = cache
        .servers
        .iter_mut()
        .find(|existing| existing.id == server.id)
        .context("服务器不存在")?;
    *existing = server.clone();
    // The edited card wins a conflict and keeps its ID and position.
    cache.servers.retain(|existing| {
        let duplicate = existing.id != server.id && same_server(existing, &server);
        if duplicate && cache.auto_start_server_id.as_deref() == Some(&existing.id) {
            cache.auto_start_server_id = Some(server.id.clone());
        }
        !duplicate
    });
    Ok(())
}

pub fn delete_server_by_id(cache: &mut ServerCatalog, id: &str) -> bool {
    let original_len = cache.servers.len();
    cache.servers.retain(|server| server.id != id);
    if cache.auto_start_server_id.as_deref() == Some(id) {
        cache.auto_start_server_id = None;
    }
    cache.servers.len() != original_len
}

fn same_server(a: &CachedServer, b: &CachedServer) -> bool {
    a.endpoint == b.endpoint && a.username == b.username
}

pub(crate) fn deduplicate_servers(cache: &mut ServerCatalog) {
    // Keep the first card and its position, including for caches saved before
    // edits checked uniqueness. Redirect auto-start if its duplicate is removed.
    let mut servers = Vec::with_capacity(cache.servers.len());
    for server in std::mem::take(&mut cache.servers) {
        if let Some(existing) = servers
            .iter()
            .find(|existing| same_server(existing, &server))
        {
            if cache.auto_start_server_id.as_deref() == Some(&server.id) {
                cache.auto_start_server_id = Some(existing.id.clone());
            }
        } else {
            servers.push(server);
        }
    }
    cache.servers = servers;
}
