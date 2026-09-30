use crate::emby::{ResumeItems, UserItems, UserViews};

use super::FeedController;

/// Borrowed read model: selectors never clone the complete response per frame.
pub(in crate::home) struct HomeFeedVm<'a> {
    pub(in crate::home) views: Option<&'a UserViews>,
    pub(in crate::home) resume: Option<&'a ResumeItems>,
    pub(in crate::home) show_views: bool,
    pub(in crate::home) show_empty_views: bool,
    pub(in crate::home) show_resume: bool,
    pub(in crate::home) missing_episode_series: bool,
    pub(in crate::home) has_content: bool,
}

pub(in crate::home) struct LatestRowVm<'a> {
    pub(in crate::home) items: Option<&'a UserItems>,
    pub(in crate::home) visible: bool,
}

impl FeedController {
    pub(in crate::home) fn latest_row(&self, view_id: &str) -> LatestRowVm<'_> {
        let items = self
            .state
            .user_view_items_rows
            .get(view_id)
            .and_then(|row| row.items.as_ref());
        LatestRowVm {
            items,
            visible: items.is_some_and(|items| !items.items.is_empty()),
        }
    }
    pub(in crate::home) fn view_model(&self) -> HomeFeedVm<'_> {
        let state = &self.state;
        let views = state.user_views.as_ref();
        let resume = state.resume_items.as_ref();
        let views_have_items = views.is_some_and(|views| !views.items.is_empty());
        let show_resume = resume.is_some_and(|items| !items.items.is_empty());
        HomeFeedVm {
            views,
            resume,
            show_views: home_data_section_is_visible(
                views.is_some(),
                views_have_items,
                state.views_load.is_loading(),
                state.user_views_failed.is_some(),
            ),
            show_empty_views: !state.views_load.is_loading()
                && state.user_views_failed.is_none()
                && !views_have_items,
            show_resume,
            missing_episode_series: resume.is_some_and(|items| {
                items.items.iter().any(|item| {
                    item.item_type.as_deref() == Some("Episode")
                        && item
                            .series_id
                            .as_deref()
                            .is_none_or(|id| id.trim().is_empty())
                })
            }),
            has_content: views_have_items
                || show_resume
                || state.user_view_items_rows.values().any(|row| {
                    row.items
                        .as_ref()
                        .is_some_and(|items| !items.items.is_empty())
                }),
        }
    }
}

pub(in crate::home) fn home_data_section_is_visible(
    has_response: bool,
    has_items: bool,
    loading: bool,
    failed: bool,
) -> bool {
    has_items || (has_response && !loading && !failed)
}
