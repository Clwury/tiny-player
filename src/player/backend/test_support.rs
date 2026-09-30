use super::*;
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

#[derive(Default)]
pub(in crate::player) struct FakeState {
    pub(in crate::player) operations: Vec<&'static str>,
    pub(in crate::player) commands: Vec<BackendCommand>,
    pub(in crate::player) events: Vec<BackendEvent>,
    pub(in crate::player) fail_commands: bool,
    pub(in crate::player) fail_volume: bool,
    pub(in crate::player) fail_load: bool,
    pub(in crate::player) fail_presenter: bool,
    pub(in crate::player) fail_pause: bool,
    pub(in crate::player) fail_resume: bool,
    pub(in crate::player) fail_render: bool,
    pub(in crate::player) frames: VecDeque<BgraImage>,
    pub(in crate::player) render_sizes: Vec<RenderSize>,
}

pub(super) struct FakeDriver(pub(super) Rc<RefCell<FakeState>>);
impl PlaybackDriver for FakeDriver {
    fn command(&mut self, command: BackendCommand) -> Result<()> {
        let mut state = self.0.borrow_mut();
        let name = match command {
            BackendCommand::SetVolume { .. } => "volume",
            BackendCommand::Load(_) => "load",
            _ => "command",
        };
        state.operations.push(name);
        let fails = state.fail_commands
            || (state.fail_pause && matches!(command, BackendCommand::Pause))
            || (state.fail_resume && matches!(command, BackendCommand::Resume))
            || (state.fail_volume && name == "volume")
            || (state.fail_load && name == "load");
        state.commands.push(command);
        if fails {
            return Err(tiny_playback::BackendError::Ffmpeg(
                "synthetic command failure".into(),
            ));
        }
        Ok(())
    }
    fn poll_events(&mut self) -> Vec<BackendEvent> {
        let mut state = self.0.borrow_mut();
        state.operations.push("poll");
        std::mem::take(&mut state.events)
    }
    fn create_presenter(&self) -> anyhow::Result<Box<dyn FramePresenter>> {
        self.0.borrow_mut().operations.push("presenter");
        if self.0.borrow().fail_presenter {
            return Err(anyhow::anyhow!("synthetic presenter failure"));
        }
        Ok(Box::new(FakePresenter(self.0.clone())))
    }
}
impl Drop for FakeDriver {
    fn drop(&mut self) {
        self.0.borrow_mut().operations.push("drop_backend");
    }
}
struct FakePresenter(Rc<RefCell<FakeState>>);
impl FramePresenter for FakePresenter {
    fn prewarm(&mut self) {
        self.0.borrow_mut().operations.push("prewarm");
    }
    fn render(&mut self, size: RenderSize) -> anyhow::Result<Option<BgraImage>> {
        let mut state = self.0.borrow_mut();
        state.render_sizes.push(size);
        state.operations.push("render");
        if state.fail_render {
            return Err(anyhow::anyhow!("synthetic render failure"));
        }
        Ok(state.frames.pop_front())
    }
    fn discard_pending(&mut self) {
        let mut state = self.0.borrow_mut();
        state.operations.push("discard");
        state.frames.clear();
    }
    fn snapshot(&self) -> VideoPresenterSnapshot {
        VideoPresenterSnapshot {
            queued: self.0.borrow().frames.len(),
            queue_capacity: 8,
            rendering: false,
            ready: false,
            pending_render_requests: 0,
            last_render_ms: 0.0,
            average_render_ms: 0.0,
            dropped_frames: 0,
            blocked_on: None,
        }
    }
}
impl Drop for FakePresenter {
    fn drop(&mut self) {
        self.0.borrow_mut().operations.push("drop_presenter");
    }
}

pub(in crate::player) fn adapter(
    state: Rc<RefCell<FakeState>>,
    presenter: bool,
) -> PlaybackBackendAdapter {
    let presenter =
        presenter.then(|| Box::new(FakePresenter(state.clone())) as Box<dyn FramePresenter>);
    PlaybackBackendAdapter::from_resources(Some(Box::new(FakeDriver(state))), presenter)
}
