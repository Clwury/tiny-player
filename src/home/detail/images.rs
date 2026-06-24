use std::path::PathBuf;

use gpui::Context;

use crate::emby::{
    EmbyImageRequest, EmbyImageType, ImageQuality, MediaItem, MediaItems, MediaPerson,
};

use super::super::HomeContent;

const SERIES_BACKDROP_IMAGE_MAX_WIDTH: u32 = 3000;
const SERIES_EPISODE_IMAGE_MAX_WIDTH: u32 = 640;
const SERIES_PERSON_IMAGE_MAX_WIDTH: u32 = 320;

impl HomeContent {
    pub(crate) fn ensure_series_media_item_images(
        &mut self,
        item: &MediaItem,
        cx: &mut Context<Self>,
    ) {
        if let Some(request) = series_backdrop_image_request(item) {
            self.ensure_image(request, cx);
        }
        if let Some(request) = series_logo_image_request(item) {
            self.ensure_image(request, cx);
        }
        if let Some(people) = item.people.as_deref() {
            for person in people {
                if let Some(request) = person_primary_image_request(person) {
                    self.ensure_image(request, cx);
                }
            }
        }
    }

    pub(crate) fn ensure_series_episode_images(
        &mut self,
        episodes: &MediaItems,
        cx: &mut Context<Self>,
    ) {
        for episode in &episodes.items {
            if let Some(request) = episode_primary_image_request(episode) {
                self.ensure_image(request, cx);
            }
        }
    }

    pub(crate) fn image_path_for_series_backdrop(&self, item: &MediaItem) -> Option<PathBuf> {
        let request = series_backdrop_image_request(item)?;
        self.image_path_for_request(&request)
    }

    pub(crate) fn image_path_for_series_logo(&self, item: &MediaItem) -> Option<PathBuf> {
        let request = series_logo_image_request(item)?;
        self.image_path_for_request(&request)
    }

    pub(crate) fn image_path_for_episode_primary(&self, episode: &MediaItem) -> Option<PathBuf> {
        let request = episode_primary_image_request(episode)?;
        self.image_path_for_request(&request)
    }

    pub(crate) fn image_path_for_person_primary(&self, person: &MediaPerson) -> Option<PathBuf> {
        let request = person_primary_image_request(person)?;
        self.image_path_for_request(&request)
    }
}

fn series_backdrop_image_request(item: &MediaItem) -> Option<EmbyImageRequest> {
    Some(
        EmbyImageRequest::new(item.id.clone(), EmbyImageType::Backdrop)
            .with_tag(Some(item.backdrop_image_tag()?.to_string()))
            .with_max_width(SERIES_BACKDROP_IMAGE_MAX_WIDTH)
            .with_quality(ImageQuality::DEFAULT),
    )
}

fn series_logo_image_request(item: &MediaItem) -> Option<EmbyImageRequest> {
    Some(
        EmbyImageRequest::new(item.id.clone(), EmbyImageType::Logo)
            .with_tag(Some(item.logo_image_tag()?.to_string()))
            .with_quality(ImageQuality::DEFAULT),
    )
}

fn episode_primary_image_request(episode: &MediaItem) -> Option<EmbyImageRequest> {
    Some(
        EmbyImageRequest::new(episode.id.clone(), EmbyImageType::Primary)
            .with_tag(Some(episode.primary_image_tag()?.to_string()))
            .with_max_width(SERIES_EPISODE_IMAGE_MAX_WIDTH)
            .with_quality(ImageQuality::DEFAULT),
    )
}

fn person_primary_image_request(person: &MediaPerson) -> Option<EmbyImageRequest> {
    Some(
        EmbyImageRequest::new(person.id()?.to_string(), EmbyImageType::Primary)
            .with_tag(Some(person.primary_image_tag()?.to_string()))
            .with_max_width(SERIES_PERSON_IMAGE_MAX_WIDTH)
            .with_quality(ImageQuality::DEFAULT),
    )
}
