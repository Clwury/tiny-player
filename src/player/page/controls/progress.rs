use super::*;

impl PlaybackPage {
    pub(in super::super) fn begin_progress_drag(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.close_track_select(cx) {
            cx.stop_propagation();
            return;
        }
        self.update_progress_drag(event.position.x, cx);
        cx.stop_propagation();
    }

    pub(in super::super) fn drag_progress(
        &mut self,
        event: &DragMoveEvent<ProgressBarDrag>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_progress_drag(event.event.position.x, cx);
        cx.stop_propagation();
    }

    pub(in super::super) fn finish_progress_drag(
        &mut self,
        _event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_progress_drag(window, cx);
        cx.stop_propagation();
    }

    pub(in super::super) fn update_progress_drag(
        &mut self,
        cursor_x: Pixels,
        cx: &mut Context<Self>,
    ) {
        let Some(position) = self.position_for_progress_cursor(cursor_x) else {
            return;
        };
        if self
            .timeline
            .progress_drag_position
            .is_none_or(|current| (current - position).abs() >= 0.02)
        {
            self.timeline.progress_drag_position = Some(position);
            cx.notify();
        }
    }

    pub(in super::super) fn commit_progress_drag(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(position) = self.timeline.progress_drag_position else {
            return;
        };
        self.seek_to_position(position, window, cx);
    }

    pub(in super::super) fn position_for_progress_cursor(&self, cursor_x: Pixels) -> Option<f64> {
        let duration = self.timeline.duration?;
        let bounds = self.timeline.progress_track_bounds?;
        let fraction = progress_fraction_for_cursor(cursor_x, bounds)?;
        Some(clamp_playback_position(
            duration * fraction as f64,
            duration,
        ))
    }
}
