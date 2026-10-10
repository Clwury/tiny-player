use std::{sync::Arc, time::Duration};

use gpui::{
    AppContext as _, Bounds, Context, DragMoveEvent, EventEmitter, FocusHandle, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, Pixels, Point, Render, RenderImage, ScrollDelta, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, Window, canvas, deferred, div, prelude::*, px, relative,
    rgb, rgba, svg,
};

use crate::{
    app::{window_corner_radii, window_uses_system_decorations},
    theme,
};

use super::image_resources::{
    SubtitleImages, defer_drop_frame, defer_drop_released_images, render_image,
};
use tiny_playback::{
    BackendCommand, BackendLoadRequest, BackendSubtitleBitmap, BackendSubtitleCue,
    PlaybackAudioInfo, PlaybackCacheState, PlaybackFileInfo, PlaybackSeekMode, PlaybackTrack,
    PlaybackTrackKind, PlaybackVideoInfo, PlaybackVolumeSettings, RenderSize, StreamCacheKind,
    VideoPresenterSnapshot, clamp_playback_volume,
};

mod backend_events;
mod controls;
mod diagnostics;
mod episodes;
mod fullscreen;
mod gamepad;
mod lifecycle;
mod power;
mod presentation;
mod progress;
mod queue;
mod rate;
mod render;
mod reporting;
mod shortcuts;
mod subtitles;
mod surface;
mod timers;
mod video_element;
mod video_viewport;
mod view;

#[cfg(test)]
mod adapter_tests;
#[cfg(test)]
mod mouse_tests;
#[cfg(test)]
mod ports_tests;
#[cfg(test)]
mod test_support;

use super::{EmbyPlaybackContext, PlaybackRequest};
#[cfg(test)]
use super::{PlaybackQueue, PlaybackQueueItem, PlaybackTrackSelection};
use crate::player::reporting::PlaybackStateUpdate;
#[cfg(test)]
use crate::player::reporting::PlaybackStopResult;

use super::backend::PlaybackBackendAdapter;
use super::model::source::{PlaybackSourceState, PlaybackTrackState, playback_protocol};
use super::model::timeline::PlaybackTimelineState;
use super::session::PlaybackIntent;
use presentation::{
    PlaybackPresentationState, PlaybackTimelinePresentation, SubtitleOverlayState, WindowDragState,
};
use progress::{
    ProgressBarDrag, clamp_playback_position, format_playback_time, progress_fraction_for_cursor,
    valid_playback_duration, valid_playback_time,
};
use render::{
    AnimationFrameRequestState, aspect_fit_bounds, normalize_video_viewport, playback_status,
    render_output_size, render_playback_status, should_render_frame,
    should_request_animation_frame, viewport_changed,
};
use subtitles::defer_drop_subtitle;
use timers::{PresentationEffects, PresentationTimer};
#[cfg(test)]
use tiny_playback::BackendEventKind;
use video_element::VideoFrameElement;
use video_viewport::VideoViewport;

#[derive(Clone, Debug)]
pub enum PlaybackEvent {
    VolumeChanged {
        settings: PlaybackVolumeSettings,
    },
    Update {
        update: PlaybackStateUpdate,
    },
    Back {
        update: PlaybackStateUpdate,
    },
    Replace {
        request: Box<PlaybackRequest>,
        update: PlaybackStateUpdate,
    },
}

impl PlaybackEvent {
    pub(crate) fn trace(&self) {
        let operation = match self {
            Self::VolumeChanged { .. } => "playback.volume_changed",
            Self::Update { .. } => "playback.update",
            Self::Back { .. } => "playback.back",
            Self::Replace { .. } => "playback.replace",
        };
        crate::observability::TraceId::start(operation).record("received");
    }
}

pub struct PlaybackPage {
    title: SharedString,
    video: PlaybackBackendAdapter,
    presentation: PlaybackPresentationState,
    session: super::session::PlaybackSessionController,
    // Page-owned continuation; the session owns poll eligibility and token.
    backend_poll: crate::effects::EffectHandle<gpui::Task<()>>,
    power: power::PlaybackPower,
    emby: EmbyPlaybackContext,
    report_effects: reporting::ReportingEffects,
    queue_effects: queue::QueueEffects,
    gamepad: gamepad::PlaybackGamepad,
}

impl EventEmitter<PlaybackEvent> for PlaybackPage {}

use surface::{PLAYBACK_VOLUME_STEP, playback_volume_percent};
