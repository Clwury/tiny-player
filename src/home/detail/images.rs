use std::{path::Path, sync::Arc};

use gpui::Context;

use crate::emby::{
    EmbyImageRequest, EmbyImageType, ImageQuality, MediaItem, MediaItems, MediaPerson,
};

use super::super::{HomeContent, data::EPISODE_CARD_IMAGE_MAX_WIDTH};

const SERIES_BACKDROP_IMAGE_MAX_WIDTH: u32 = 1024;
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

    pub(crate) fn image_path_for_series_backdrop(&self, item: &MediaItem) -> Option<Arc<Path>> {
        let request = series_backdrop_image_request(item)?;
        self.image_path_for_request(&request)
    }

    pub(crate) fn image_path_for_series_logo(&self, item: &MediaItem) -> Option<Arc<Path>> {
        let request = series_logo_image_request(item)?;
        self.image_path_for_request(&request)
    }

    pub(crate) fn image_path_for_episode_primary(&self, episode: &MediaItem) -> Option<Arc<Path>> {
        let request = episode_primary_image_request(episode)?;
        self.image_path_for_request(&request)
    }

    pub(crate) fn image_path_for_person_primary(&self, person: &MediaPerson) -> Option<Arc<Path>> {
        let request = person_primary_image_request(person)?;
        self.image_path_for_request(&request)
    }
}

fn series_backdrop_image_request(item: &MediaItem) -> Option<EmbyImageRequest> {
    let (image_type, tag) = if let Some(tag) = item.backdrop_image_tag() {
        (EmbyImageType::Backdrop, tag)
    } else {
        (EmbyImageType::Primary, item.primary_image_tag()?)
    };

    Some(
        EmbyImageRequest::new(item.id.clone(), image_type)
            .with_tag(Some(tag.to_string()))
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
            .with_max_width(EPISODE_CARD_IMAGE_MAX_WIDTH)
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn hero_prefers_backdrop_when_primary_is_also_available() {
        let item: MediaItem = serde_json::from_value(json!({
            "Id": "movie-1",
            "Name": "Movie",
            "BackdropImageTags": ["", "backdrop-tag"],
            "ImageTags": {"Primary": "primary-tag"}
        }))
        .unwrap();

        assert_eq!(
            series_backdrop_image_request(&item),
            Some(
                EmbyImageRequest::new("movie-1", EmbyImageType::Backdrop)
                    .with_tag(Some("backdrop-tag".into()))
                    .with_max_width(SERIES_BACKDROP_IMAGE_MAX_WIDTH)
            )
        );
    }

    #[test]
    fn hero_falls_back_to_primary_when_backdrop_tags_are_missing_or_empty() {
        for tags in [json!(null), json!([]), json!(["", " "])] {
            let item: MediaItem = serde_json::from_value(json!({
                "Id": "movie-1",
                "Name": "Movie",
                "BackdropImageTags": tags,
                "ImageTags": {"Primary": "primary-tag"}
            }))
            .unwrap();

            assert_eq!(
                series_backdrop_image_request(&item),
                Some(
                    EmbyImageRequest::primary("movie-1", Some("primary-tag".into()))
                        .with_max_width(SERIES_BACKDROP_IMAGE_MAX_WIDTH)
                )
            );
        }
    }

    #[test]
    fn hero_skips_image_request_when_backdrop_and_primary_are_unavailable() {
        for tags in [json!(null), json!({}), json!({"Primary": " "})] {
            let item: MediaItem = serde_json::from_value(json!({
                "Id": "movie-1",
                "Name": "Movie",
                "BackdropImageTags": [],
                "ImageTags": tags
            }))
            .unwrap();

            assert_eq!(series_backdrop_image_request(&item), None);
        }
    }
}
