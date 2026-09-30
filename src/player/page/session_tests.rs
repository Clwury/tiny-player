use super::*;
use crate::player::gateway::{PlaybackGateway, PlaybackReport, ResolvedPlayback};
use std::sync::mpsc;

#[derive(Debug, PartialEq)]
enum Report {
    Started {
        position: u64,
    },
    Progress {
        position: u64,
        paused: bool,
        muted: bool,
    },
    Stopped {
        position: u64,
    },
    Released,
}

struct FakeGateway(mpsc::Sender<Report>);
impl PlaybackGateway for FakeGateway {
    fn report(&self, report: &PlaybackReport) -> anyhow::Result<()> {
        let report = match report {
            PlaybackReport::Started(report) => Report::Started {
                position: report.position_ticks,
            },
            PlaybackReport::Progress(report) => Report::Progress {
                position: report.position_ticks,
                paused: report.is_paused,
                muted: report.is_muted,
            },
            PlaybackReport::Stopped(report) => Report::Stopped {
                position: report.position_ticks,
            },
        };
        self.0.send(report).unwrap();
        Ok(())
    }
    fn resolve_source(&self, _: &str, _: &str) -> anyhow::Result<ResolvedPlayback> {
        unreachable!()
    }
    fn subtitle_tracks(
        &self,
        _: &crate::emby::MediaSource,
        _: &str,
        _: &str,
    ) -> Vec<PlaybackTrack> {
        unreachable!()
    }
}
impl Drop for FakeGateway {
    fn drop(&mut self) {
        let _ = self.0.send(Report::Released);
    }
}

fn install(
    page: &gpui::Entity<PlaybackPage>,
    cx: &mut gpui::VisualTestContext,
) -> mpsc::Receiver<Report> {
    let (tx, rx) = mpsc::channel();
    page.update(cx, |page, cx| {
        page.report_effects.reporter = super::super::report_delivery::ReportDelivery::new(
            PlaybackReporter::new(
                Arc::new(FakeGateway(tx)),
                page.emby.server.workspace_identity(),
            ),
            cx.foreground_executor().clone(),
        );
    });
    rx
}
#[track_caller]
fn recv(rx: &mpsc::Receiver<Report>) -> Report {
    rx.recv_timeout(Duration::from_secs(5)).unwrap()
}

#[gpui::test]
fn page_timer_repeats_at_ten_seconds_deduplicates_and_cancels_at_close(
    cx: &mut gpui::TestAppContext,
) {
    let (page, cx) = episodes::tests::playback_window(cx);
    let rx = install(&page, cx);
    page.update(cx, |page, cx| page.handle_playback_restart_reporting(cx));
    cx.run_until_parked();
    assert_eq!(
        recv(&rx),
        Report::Started {
            position: 450_000_000
        }
    );
    cx.run_until_parked();
    page.update(cx, |page, _| {
        page.session.timeline_mut().position = Some(50.0)
    });
    cx.executor().advance_clock(Duration::from_secs(9));
    cx.run_until_parked();
    assert!(rx.try_recv().is_err());
    cx.executor().advance_clock(Duration::from_secs(1));
    cx.run_until_parked();
    assert_eq!(
        recv(&rx),
        Report::Progress {
            position: 500_000_000,
            paused: false,
            muted: false
        }
    );
    cx.executor().advance_clock(PROGRESS_INTERVAL);
    cx.run_until_parked();
    assert!(rx.try_recv().is_err());
    page.update(cx, |page, _| {
        page.session.timeline_mut().user_paused = true;
        page.session.timeline_mut().paused = false; // Backend pause notification may lag user intent.
        page.session
            .dispatch_control(crate::player::session::PlaybackIntent::ToggleMute, |_| None);
        page.report_playback_progress(false);
    });
    cx.run_until_parked();
    assert_eq!(
        recv(&rx),
        Report::Progress {
            position: 500_000_000,
            paused: true,
            muted: true
        }
    );
    let receipt = page.update(cx, |page, _| {
        page.close_playback_reporting(false, false)
            .stop_completion
            .unwrap()
    });
    cx.run_until_parked();
    assert_eq!(
        recv(&rx),
        Report::Stopped {
            position: 500_000_000
        }
    );
    assert_eq!(recv(&rx), Report::Released);
    assert_eq!(receipt.result(), PlaybackStopResult::Succeeded);
    page.update(cx, |page, cx| {
        page.session.timeline_mut().position = Some(60.0);
        page.handle_playback_restart_reporting(cx);
        page.report_playback_progress(true);
    });
    cx.executor().advance_clock(PROGRESS_INTERVAL * 3);
    cx.run_until_parked();
    assert!(rx.try_recv().is_err());
}

#[gpui::test]
fn page_release_cancels_timer_and_drains_exactly_one_stop(cx: &mut gpui::TestAppContext) {
    let (page, cx) = episodes::tests::playback_window(cx);
    let rx = install(&page, cx);
    page.update(cx, |page, cx| page.handle_playback_restart_reporting(cx));
    cx.run_until_parked();
    assert_eq!(
        recv(&rx),
        Report::Started {
            position: 450_000_000
        }
    );
    cx.run_until_parked();
    let weak = page.downgrade();
    cx.update(|window, _| {
        window.remove_window();
        drop(page);
    });
    cx.run_until_parked();
    assert!(weak.upgrade().is_none());
    assert_eq!(
        recv(&rx),
        Report::Stopped {
            position: 450_000_000
        }
    );
    assert_eq!(recv(&rx), Report::Released);
    cx.executor().advance_clock(PROGRESS_INTERVAL * 2);
    cx.run_until_parked();
    assert!(rx.try_recv().is_err());
}
