//! Shared Home components grouped by presentation responsibility.
use std::{path::Path, sync::Arc};

use gpui::{
    Animation, AnimationExt as _, App, Bounds, ClickEvent, ContentMask, Context,
    InteractiveElement, IntoElement, MouseButton, ParentElement, StatefulInteractiveElement,
    Styled, StyledImage, Window, canvas, div, ease_in_out, fill, img, point,
    prelude::FluentBuilder, px, size, svg,
};

use super::model::cards::{
    EpisodeCardVm, PersonCardVm, PosterBadgesVm, ResumeCardVm, UserEpisodeCardVm, UserItemCardVm,
};
use crate::emby::UserItem;
use crate::{
    images::cover::{CoverImageAsset, CoverImageRequest},
    ui::radius,
};
use crate::{theme, ui::tooltip::text_tooltip};

use super::carousel::{
    CAROUSEL_SCROLL_DURATION, DETAIL_EPISODE_CARD_IMAGE_HEIGHT_PX, DETAIL_EPISODE_CARD_PADDING_PX,
    DETAIL_EPISODE_CARD_WIDTH_PX, DETAIL_PERSON_CARD_IMAGE_HEIGHT_PX,
    DETAIL_PERSON_CARD_IMAGE_WIDTH_PX, DETAIL_PERSON_CARD_PADDING_PX, DETAIL_PERSON_CARD_WIDTH_PX,
    HOME_ITEM_CARD_IMAGE_HEIGHT_PX, HOME_ITEM_CARD_PADDING_PX, HOME_ITEM_CARD_WIDTH_PX,
    USER_VIEW_CARD_IMAGE_HEIGHT_PX, USER_VIEW_CARD_PADDING_PX, USER_VIEW_CARD_WIDTH_PX,
};

const IMAGE_PROGRESS_BAR_HEIGHT_PX: f32 = 4.0;
const IMAGE_PROGRESS_BAR_HORIZONTAL_INSET_PX: f32 = 8.0;

mod episode;
mod images;
mod navigation;
mod person;
mod poster;
pub(in crate::home) use episode::{episode_card, favorite_episode_card, user_episode_card};
pub(in crate::home) use images::cover_img;
use images::{cover_image_progress_bar, image_progress_bar};
pub(in crate::home) use navigation::{
    carousel_button, home_carousel_track, home_section_more_button, home_section_title,
    home_section_title_text, tallest_home_item, workspace_back_button,
};
pub(in crate::home) use person::person_card;
pub(in crate::home) use poster::{resume_item_card, user_item_card, user_view_card};
#[cfg(test)]
mod tests;
