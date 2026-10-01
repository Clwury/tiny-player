//! Shared GPUI workspace fixture, without starting feed IO.
use super::*;

pub(in crate::home) fn content(cx: &mut gpui::TestAppContext) -> gpui::Entity<HomeContent> {
    let server = serde_json::from_value(serde_json::json!({
        "id": "local", "server_id": "remote", "user_id": "user",
        "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
        "username": "test", "password": "", "added_at_unix": 0
    }))
    .unwrap();
    cx.new(|cx| {
        HomeContent::new(
            server,
            crate::emby::EmbyClient::new("test".into()).unwrap(),
            cx,
        )
    })
}

pub(super) fn ports(
    server: &crate::server::CachedServer,
    client: &crate::emby::EmbyClient,
) -> super::HomePorts {
    super::HomePorts::new(
        std::sync::Arc::new(super::adapter::EmbyHomeGateway {
            server: server.clone(),
            client: client.clone(),
        }),
        std::sync::Arc::new(crate::player::adapter::EmbyPlaybackGateway {
            server: server.clone(),
            client: client.clone(),
        }),
    )
}
