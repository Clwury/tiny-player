pub(crate) mod subtitles;

use super::gateway::{PlaybackGateway, ResolvedPlayback};
use crate::{
    emby::{EmbyClient, MediaSource, playback::resolve_direct_stream_url},
    server::CachedServer,
};

/// Immutable account snapshot owned by a playback effect. Acceptance remains
/// the controller's responsibility, including selection and navigation changes.
pub(crate) struct EmbyPlaybackGateway {
    pub(crate) client: EmbyClient,
    pub(crate) server: CachedServer,
}

impl PlaybackGateway for EmbyPlaybackGateway {
    fn report(&self, report: &super::reporting::PlaybackReport) -> anyhow::Result<()> {
        use super::reporting::PlaybackReport;
        match report {
            PlaybackReport::Started(report) => {
                self.client.report_playback_started(&self.server, report)
            }
            PlaybackReport::Progress(report) => {
                self.client.report_playback_progress(&self.server, report)
            }
            PlaybackReport::Stopped(report) => {
                self.client.report_playback_stopped(&self.server, report)
            }
        }
    }

    fn resolve_source(
        &self,
        item_id: &str,
        media_source_id: &str,
    ) -> anyhow::Result<ResolvedPlayback> {
        let info = self
            .client
            .playback_info(&self.server, item_id, media_source_id)?;
        let source = info.direct_stream_source_for(media_source_id)?;
        let url = resolve_direct_stream_url(&self.server, source.direct_stream_url()?)?;
        let http_headers = self.client.playback_http_headers(&self.server)?;
        Ok(ResolvedPlayback {
            item_id: source.playback_item_id(item_id).to_string(),
            url: url.to_string(),
            http_headers,
            content_length: source.size,
            media_source_id: source
                .id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .unwrap_or(media_source_id)
                .to_string(),
            play_session_id: info.play_session_id,
        })
    }

    fn subtitle_tracks(
        &self,
        source: &MediaSource,
        item_id: &str,
        media_source_id: &str,
    ) -> Vec<super::PlaybackTrack> {
        subtitles::playback_subtitle_tracks_for_source(
            source,
            &self.server,
            item_id,
            media_source_id,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        time::Duration,
    };

    #[test]
    fn source_resolution_preserves_request_query_grouped_identity_and_transport_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let response = serde_json::json!({
            "PlaySessionId":"session",
            "MediaSources":[
                {"Id":"other", "ItemId":"wrong", "DirectStreamUrl":"/wrong"},
                {"Id":"selected", "ItemId":"resolved-item", "Size":12_345, "DirectStreamUrl":"/emby/Videos/resolved-item/stream?Static=true"}
            ]
        }).to_string();
        let mock = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line);
            }
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            write!(reader.get_mut(), "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
            (
                headers.lines().next().unwrap().to_owned(),
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            )
        });
        let server = serde_json::from_value(serde_json::json!({
            "id":"gateway-test", "username":"test", "password":"", "user_id":"user", "access_token":"synthetic-token", "added_at_unix":0,
            "endpoint":{"protocol":"Http", "address":"127.0.0.1", "port":port, "path":""}
        })).unwrap();
        let gateway = EmbyPlaybackGateway {
            client: EmbyClient::new("test".into()).unwrap(),
            server,
        };
        let result = gateway.resolve_source("physical-item", "selected").unwrap();
        let (target, body) = mock.join().unwrap();
        assert!(target.starts_with("POST /emby/Items/physical-item/PlaybackInfo?"));
        assert!(target.contains("MediaSourceId=selected") && target.contains("UserId=user"));
        assert!(target.contains("IsPlayback=false") && target.contains("AutoOpenLiveStream=false"));
        assert_eq!(
            body,
            serde_json::to_value(crate::player::device_profile()).unwrap()
        );
        assert_eq!(result.item_id, "resolved-item");
        assert_eq!(result.media_source_id, "selected");
        assert_eq!(result.content_length, Some(12_345));
        assert_eq!(result.play_session_id.as_deref(), Some("session"));
        assert_eq!(
            result.url,
            format!("http://127.0.0.1:{port}/emby/Videos/resolved-item/stream?Static=true")
        );
        assert_eq!(
            result.http_headers,
            gateway
                .client
                .playback_http_headers(&gateway.server)
                .unwrap()
        );
    }
}

#[cfg(test)]
#[path = "adapter_reporting_tests.rs"]
mod reporting_tests;
