use crate::emby::{UserView, VideoItemType};

pub(crate) fn library_item_types(collection_type: Option<&str>) -> Option<Vec<VideoItemType>> {
    match normalized_collection_type(collection_type).as_deref() {
        Some("movies") => Some(vec![VideoItemType::Movie]),
        Some("tvshows") => Some(vec![VideoItemType::Series]),
        Some("mixed") | None => Some(vec![VideoItemType::Movie, VideoItemType::Series]),
        Some(collection_type) if known_unsupported_collection(collection_type) => None,
        Some(_) => Some(vec![VideoItemType::Movie, VideoItemType::Series]),
    }
}

pub(crate) fn latest_item_types(collection_type: Option<&str>) -> Option<Vec<VideoItemType>> {
    match normalized_collection_type(collection_type).as_deref() {
        Some("movies") => Some(vec![VideoItemType::Movie]),
        Some("tvshows") => Some(vec![VideoItemType::Series, VideoItemType::Episode]),
        Some("mixed") | None => Some(vec![
            VideoItemType::Movie,
            VideoItemType::Series,
            VideoItemType::Episode,
        ]),
        Some(collection_type) if known_unsupported_collection(collection_type) => None,
        Some(_) => Some(vec![
            VideoItemType::Movie,
            VideoItemType::Series,
            VideoItemType::Episode,
        ]),
    }
}

pub(crate) fn is_supported_view(view: &UserView) -> bool {
    !view.id.trim().is_empty() && library_item_types(view.collection_type.as_deref()).is_some()
}

fn normalized_collection_type(collection_type: Option<&str>) -> Option<String> {
    collection_type
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
}

fn known_unsupported_collection(collection_type: &str) -> bool {
    matches!(
        collection_type,
        "music"
            | "musicvideos"
            | "audiobooks"
            | "books"
            | "boxsets"
            | "playlists"
            | "homevideos"
            | "homevideosandphotos"
            | "photos"
            | "trailers"
            | "folders"
            | "games"
            | "livetv"
            | "channels"
    )
}
