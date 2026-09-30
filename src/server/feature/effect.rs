use super::gateway::ServerGateway;
use crate::server::{AddServerSubmission, CachedServer};
use anyhow::{Result, anyhow, ensure};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub(crate) fn prepare_server(
    client: &impl ServerGateway,
    submission: &AddServerSubmission,
    existing: Option<&CachedServer>,
) -> Result<CachedServer> {
    if let Some(existing) = existing {
        return Ok(CachedServer {
            endpoint: submission.endpoint.clone(),
            username: submission.username.clone(),
            password: submission.password.clone(),
            needs_auth_refresh: true,
            ..existing.clone()
        });
    }
    let info = client.public_system_info(submission)?;
    let icon_url = info
        .server_name
        .as_deref()
        .and_then(|name| client.matched_icon_url(name));
    Ok(CachedServer {
        id: Uuid::new_v4().to_string(),
        endpoint: submission.endpoint.clone(),
        username: submission.username.clone(),
        password: submission.password.clone(),
        user_id: None,
        server_id: info.id,
        server_name: info.server_name,
        icon_url,
        icon_is_custom: false,
        access_token: None,
        needs_auth_refresh: false,
        item_counts: None,
        added_at_unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    })
}

pub(crate) fn authenticate_server(
    client: &impl ServerGateway,
    server: &CachedServer,
) -> Result<CachedServer> {
    let submission = AddServerSubmission {
        endpoint: server.endpoint.clone(),
        username: server.username.clone(),
        password: server.password.clone(),
    };
    let session = client.authenticate_by_name(&submission)?;
    let user_id = session
        .user_id()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| anyhow!("Emby 认证响应缺少用户 ID"))?;
    ensure!(
        !session.access_token.trim().is_empty(),
        "Emby 认证响应缺少访问令牌"
    );

    // Public metadata fetched on add is reusable. After edits it may describe
    // a different endpoint, so refresh it on entry when authentication omits it.
    let server_name = session.server_name().or_else(|| {
        server
            .server_name
            .clone()
            .filter(|name| !server.needs_auth_refresh && !name.trim().is_empty())
    });
    let info = if server_name.is_none() {
        client.public_system_info(&submission).ok()
    } else {
        None
    };
    let server_name = server_name
        .or_else(|| info.as_ref().and_then(|info| info.server_name.clone()))
        .or_else(|| server.server_name.clone());
    let icon_url = if server.icon_is_custom {
        server.icon_url.clone()
    } else {
        server_name
            .as_deref()
            .and_then(|name| client.matched_icon_url(name))
    };

    Ok(CachedServer {
        user_id: Some(user_id),
        server_id: session
            .server_id()
            .or_else(|| info.and_then(|info| info.id))
            .or_else(|| server.server_id.clone()),
        server_name,
        icon_url,
        access_token: Some(session.access_token),
        needs_auth_refresh: false,
        ..server.clone()
    })
}

pub(crate) fn download_icon(
    images: &impl crate::images::ImageRepository<
        crate::images::ServerIconRequest,
        Image = std::sync::Arc<gpui::RenderImage>,
    >,
    request: &super::IconRequest,
) -> Result<()> {
    images
        .load(&crate::images::ServerIconRequest {
            url: request.url.clone(),
            load: crate::images::ServerIconLoad::Cached,
        })
        .map(|_| ())
}
