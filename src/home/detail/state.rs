//! A detail entry keeps business ownership and GPUI resources explicit.
use super::controller::{DetailController, DetailId};
use crate::effects::{DetailResource, EffectHandle};
use crate::home::model::navigation::NavigationChange;
use std::collections::HashMap;

use crate::home::{
    carousel::{CarouselState, DETAIL_EPISODE_CARD_STEP_PX},
    model::detail::{DetailChange, SeriesDetailModel},
};
use gpui::{ScrollHandle, point, px};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeriesDetailSelectKind {
    Season,
    MediaSource,
    Subtitle,
    Actions,
}

/// Owned by the detail view/history entry. Input and layout write handles;
/// selection outcomes reset menus/offsets; releasing the entry drops references.
#[derive(Clone, Debug, Default)]
pub(crate) struct SeriesDetailPresentation {
    pub(crate) open_select: Option<SeriesDetailSelectKind>,
    pub(crate) action_menu_focus: Option<gpui::FocusHandle>,
    pub(in crate::home) overview_overlay: Option<super::overview::MovieOverviewOverlay>,
    pub(crate) scroll_handle: ScrollHandle,
    pub(crate) season_scroll_handle: ScrollHandle,
    pub(crate) media_source_scroll_handle: ScrollHandle,
    pub(crate) subtitle_scroll_handle: ScrollHandle,
    pub(crate) episodes_carousel: CarouselState,
    pub(crate) people_carousel: CarouselState,
    pub(crate) similar_carousel: CarouselState,
}

/// GPUI resources are keyed by stable DetailId, independently of navigation's
/// model stack. Hiding cancels tasks; removing returns resources for deferred drop.
#[derive(Debug, Default)]
pub(crate) struct DetailResources {
    pub(crate) playback_task: EffectHandle<gpui::Task<()>>,
    pub(crate) tasks: HashMap<DetailResource, EffectHandle<gpui::Task<()>>>,
    pub(crate) presentation: SeriesDetailPresentation,
}
impl DetailResources {
    fn deactivate(&mut self) {
        self.tasks.clear();
        self.playback_task.cancel();
        self.presentation.overview_overlay = None;
    }
}

#[derive(Clone, Copy)]
pub(crate) struct DetailView<'a> {
    pub(crate) model: &'a SeriesDetailModel,
    pub(crate) presentation: &'a SeriesDetailPresentation,
}
/// A runner can mutate GPUI resources while borrowing the business model read-only.
pub(crate) struct DetailBinding<'a> {
    pub(crate) model: &'a SeriesDetailModel,
    pub(crate) playback_task: &'a mut EffectHandle<gpui::Task<()>>,
    pub(crate) tasks: &'a mut HashMap<DetailResource, EffectHandle<gpui::Task<()>>>,
    pub(crate) presentation: &'a mut SeriesDetailPresentation,
}
pub(crate) fn detail_view<'a>(
    detail: Option<crate::home::controller::DetailModelView<'a>>,
    resources: &'a HashMap<DetailId, DetailResources>,
) -> Option<DetailView<'a>> {
    let detail = detail?;
    let presentation = &resources.get(&detail.id)?.presentation;
    Some(DetailView {
        model: detail.model,
        presentation,
    })
}

pub(crate) fn detail_binding<'a>(
    detail: Option<crate::home::controller::DetailModelView<'a>>,
    resources: &'a mut HashMap<DetailId, DetailResources>,
) -> Option<DetailBinding<'a>> {
    let detail = detail?;
    let resources = resources.get_mut(&detail.id)?;
    Some(DetailBinding {
        model: detail.model,
        playback_task: &mut resources.playback_task,
        tasks: &mut resources.tasks,
        presentation: &mut resources.presentation,
    })
}

/// Called after a pure navigation transition, before scheduling the next IO.
/// Removed resources and domain models stay paired until their original release point.
pub(crate) fn apply_navigation_change(
    change: NavigationChange,
    resources: &mut HashMap<DetailId, DetailResources>,
) -> (Vec<DetailController>, Vec<DetailResources>) {
    if let Some(id) = change.hidden
        && let Some(resources) = resources.get_mut(&id)
    {
        resources.deactivate();
    }
    let removed_resources = change
        .removed
        .iter()
        .filter_map(|detail| resources.remove(&detail.id()))
        .map(|mut resources| {
            resources.deactivate();
            resources
        })
        .collect();
    (change.removed, removed_resources)
}

impl SeriesDetailPresentation {
    pub(crate) fn apply_change(&mut self, change: DetailChange) {
        let view = self;
        if change.episode_changed || change.episodes_reset {
            let origin = point(px(0.0), px(0.0));
            view.media_source_scroll_handle.set_offset(origin);
            view.subtitle_scroll_handle.set_offset(origin);
        }
        if change.source_selected {
            view.subtitle_scroll_handle
                .set_offset(point(px(0.0), px(0.0)));
        }
        if change.episode_changed
            || change.episodes_reset
            || change.source_selected
            || change.selection_unavailable
            || (change.subtitles_unavailable
                && view.open_select == Some(SeriesDetailSelectKind::Subtitle))
        {
            view.open_select = None;
        }
        if change.episodes_reset {
            view.episodes_carousel = CarouselState::default();
        }
        if let Some(index) = change.reveal_episode {
            view.episodes_carousel
                .set_scroll_offset(index as f32 * DETAIL_EPISODE_CARD_STEP_PX, f32::INFINITY);
            view.episodes_carousel.sync_previous_offset();
        }
    }
}
