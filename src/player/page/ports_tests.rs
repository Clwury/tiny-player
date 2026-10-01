use super::*;
use crate::{
    media::gateway::{PlaybackSourceGateway, ResolvedPlayback},
    player::{
        PlaybackPorts,
        reporting::{PlaybackReport, gateway::PlaybackReportGateway},
    },
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

#[derive(Default)]
struct Source {
    fail: AtomicBool,
    calls: Mutex<Vec<(String, String)>>,
}

impl PlaybackSourceGateway for Source {
    fn resolve_source(&self, item: &str, source: &str) -> anyhow::Result<ResolvedPlayback> {
        self.calls
            .lock()
            .unwrap()
            .push((item.into(), source.into()));
        anyhow::ensure!(!self.fail.load(Ordering::SeqCst), "injected source failure");
        Ok(ResolvedPlayback {
            item_id: format!("{item}-physical"),
            media_source_id: source.into(),
            url: "https://example.invalid/injected".into(),
            http_headers: vec![("Synthetic".into(), "header".into())],
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
        vec![]
    }
}

struct Reporter(mpsc::Sender<&'static str>);
impl PlaybackReportGateway for Reporter {
    fn report(&self, report: &PlaybackReport) -> anyhow::Result<()> {
        let kind = match report {
            PlaybackReport::Started(_) => "started",
            PlaybackReport::Progress(_) => "progress",
            PlaybackReport::Stopped(_) => "stopped",
        };
        let _ = self.0.send(kind);
        Ok(())
    }
}
impl Drop for Reporter {
    fn drop(&mut self) {
        let _ = self.0.send("released");
    }
}

#[gpui::test]
fn injected_source_drives_queue_failure_and_replacement(cx: &mut gpui::TestAppContext) {
    cx.update(theme::init);
    let source = Arc::new(Source::default());
    source.fail.store(true, Ordering::SeqCst);
    let (tx, _rx) = mpsc::channel();
    let ports = PlaybackPorts::new(source.clone(), Arc::new(Reporter(tx)));
    let (page, cx) = cx.add_window_view(|_, cx| PlaybackPage::test_fixture_with_ports(ports, cx));
    let events = Rc::new(RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        let events = events.clone();
        cx.subscribe(&page, move |_, event: &PlaybackEvent, _| {
            events.borrow_mut().push(event.clone())
        })
        .detach();
    });
    cx.update(|window, cx| page.update(cx, |page, cx| page.switch_to_episode(2, window, cx)));
    cx.run_until_parked();
    page.read_with(cx, |page, _| {
        assert_eq!(
            page.session.queue.view_model().error,
            Some("切换剧集失败：injected source failure")
        );
        assert_eq!(page.session.queue.queue().current_index, 0);
    });
    assert!(events.borrow().is_empty());

    source.fail.store(false, Ordering::SeqCst);
    cx.update(|window, cx| page.update(cx, |page, cx| page.switch_to_episode(2, window, cx)));
    cx.run_until_parked();
    assert_eq!(
        *source.calls.lock().unwrap(),
        vec![
            ("episode-2".to_owned(), "source-2".to_owned()),
            ("episode-2".to_owned(), "source-2".to_owned()),
        ]
    );
    let events = events.borrow();
    let [PlaybackEvent::Replace { request, .. }] = events.as_slice() else {
        panic!("one replacement expected")
    };
    assert_eq!(request.emby.item_id, "episode-2-physical");
    assert_eq!(
        request.emby.play_session_id.as_deref(),
        Some("injected-session")
    );
    assert_eq!(request.url, "https://example.invalid/injected");
    assert_eq!(request.content_length, Some(123));
    assert_eq!(
        request.http_headers,
        vec![("Synthetic".to_owned(), "header".to_owned())]
    );
}

#[gpui::test]
fn injected_reporter_delivers_accepted_stop_after_page_release(cx: &mut gpui::TestAppContext) {
    cx.update(theme::init);
    let (tx, rx) = mpsc::channel();
    let ports = PlaybackPorts::new(Arc::new(Source::default()), Arc::new(Reporter(tx)));
    let page = cx.new(|cx| PlaybackPage::test_fixture_with_ports(ports, cx));
    page.update(cx, |page, cx| page.handle_playback_restart_reporting(cx));
    cx.run_until_parked();
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "started");
    let receipt = page.update(cx, |page, _| {
        page.close_playback_reporting(false, false)
            .stop_completion
            .unwrap()
    });
    drop(page);
    cx.run_until_parked();
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "stopped");
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), "released");
    assert_eq!(
        receipt.result(),
        crate::player::PlaybackStopResult::Succeeded
    );
    assert!(rx.try_recv().is_err());
}
