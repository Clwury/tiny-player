use super::*;
use crate::{
    home::{HomePorts, detail::test_fixture::DetailFixture},
    media::gateway::PlaybackSourceGateway,
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct SourcePort {
    requests: Mutex<Vec<(String, String)>>,
}

impl PlaybackSourceGateway for SourcePort {
    fn resolve_source(&self, item: &str, source: &str) -> anyhow::Result<ResolvedPlayback> {
        self.requests
            .lock()
            .unwrap()
            .push((item.into(), source.into()));
        Ok(ResolvedPlayback {
            item_id: item.into(),
            media_source_id: source.into(),
            url: "https://example.invalid/injected-source".into(),
            http_headers: Vec::new(),
            content_length: Some(123),
            play_session_id: Some("injected-session".into()),
        })
    }

    fn subtitle_tracks(
        &self,
        _: &crate::emby::MediaSource,
        _: &str,
        _: &str,
    ) -> Vec<PlaybackTrack> {
        Vec::new()
    }
}

#[gpui::test]
fn detail_launch_uses_the_injected_source_port_for_the_current_workspace(
    cx: &mut gpui::TestAppContext,
) {
    let server: crate::server::CachedServer = serde_json::from_value(serde_json::json!({
        "id":"local", "server_id":"remote", "user_id":"user",
        "endpoint":{"protocol":"Https", "address":"example.invalid", "port":443, "path":""},
        "username":"test", "password":"", "added_at_unix":0
    }))
    .unwrap();
    let client = crate::emby::EmbyClient::new("test".into()).unwrap();
    let browsing = crate::home::test_support::ports(&server, &client).browsing;
    let gateway = Arc::new(SourcePort::default());
    let page = cx.new(|cx| {
        HomeContent::with_ports(
            server.clone(),
            client,
            HomePorts::new(browsing, gateway.clone()),
            cx,
        )
    });
    let opened = Rc::new(RefCell::new(None));
    let subscription = cx.update(|cx| {
        let opened = opened.clone();
        cx.subscribe(&page, move |_, event: &HomeContentEvent, _| {
            if let HomeContentEvent::OpenPlayback(request) = event {
                opened.replace(Some(request.clone()));
            }
        })
    });
    page.update(cx, |page, cx| {
        let item = serde_json::from_value(serde_json::json!({
            "Id":"movie", "Name":"Movie", "Type":"Movie"
        }))
        .unwrap();
        let mut detail = DetailFixture::from_user_item(&item, page.request_identity()).unwrap();
        detail.controller.state.item = Some(
            serde_json::from_value(serde_json::json!({
                "Id":"movie", "Name":"Movie", "Type":"Movie",
                "MediaSources":[{"Id":"source", "ItemId":"physical", "Type":"Grouping"}]
            }))
            .unwrap(),
        );
        detail.controller.state.sync_media_source_selection();
        page.install_detail_fixture(Some(detail));
        page.launch_selected_media(cx);
    });
    cx.run_until_parked();
    let request = opened.borrow();
    let request = request
        .as_ref()
        .expect("the injected port resolves playback");
    assert_eq!(request.url, "https://example.invalid/injected-source");
    assert_eq!(
        request.emby.server.workspace_identity(),
        server.workspace_identity()
    );
    assert_eq!(
        request.emby.play_session_id.as_deref(),
        Some("injected-session")
    );
    assert_eq!(
        *gateway.requests.lock().unwrap(),
        [("physical".into(), "source".into())]
    );
    drop(subscription);
}
