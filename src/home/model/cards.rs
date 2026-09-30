//! Small owned display values for cards. User-data precedence is resolved by
//! HomeController before projection; no catalog/media record is cloned for UI.
use crate::emby::{MediaItem, MediaPerson, ResumeItem, UserItem, UserItemData};

pub(in crate::home) struct ResumeCardVm {
    pub(in crate::home) title: String,
    pub(in crate::home) subtitle: Option<String>,
    pub(in crate::home) played_fraction: Option<f32>,
    pub(in crate::home) favorite: bool,
}

pub(in crate::home) struct PosterBadgesVm {
    pub(in crate::home) rating: Option<String>,
    pub(in crate::home) unplayed_count: Option<u32>,
    pub(in crate::home) favorite: bool,
}

pub(in crate::home) struct UserItemCardVm {
    pub(in crate::home) title: String,
    pub(in crate::home) year: Option<String>,
    pub(in crate::home) badges: PosterBadgesVm,
}

pub(in crate::home) struct UserEpisodeCardVm {
    pub(in crate::home) title: String,
    pub(in crate::home) subtitle: String,
    pub(in crate::home) played_fraction: Option<f32>,
    pub(in crate::home) played: bool,
}

pub(in crate::home) struct EpisodeCardVm {
    pub(in crate::home) label: String,
    pub(in crate::home) overview: Option<String>,
    pub(in crate::home) selected: bool,
    pub(in crate::home) played_fraction: Option<f32>,
    pub(in crate::home) played: bool,
}

pub(in crate::home) struct PersonCardVm {
    pub(in crate::home) name: String,
    pub(in crate::home) role: String,
    pub(in crate::home) kind: String,
}

impl ResumeCardVm {
    pub(in crate::home) fn new(item: &ResumeItem, data: Option<&UserItemData>) -> Self {
        let (title, subtitle) = resume_item_card_text(item);
        Self {
            title,
            subtitle,
            played_fraction: played_fraction(data),
            favorite: data.is_some_and(|data| data.is_favorite),
        }
    }
}

impl UserItemCardVm {
    pub(in crate::home) fn new(
        item: &UserItem,
        data: Option<&UserItemData>,
        show_favorite: bool,
    ) -> Self {
        Self {
            title: item.name.clone(),
            year: item.production_year.map(|year| year.to_string()),
            badges: PosterBadgesVm {
                rating: item.community_rating.map(format_community_rating),
                unplayed_count: data
                    .and_then(|data| data.unplayed_item_count)
                    .filter(|count| *count > 0),
                favorite: show_favorite && data.is_some_and(|data| data.is_favorite),
            },
        }
    }
}

impl UserEpisodeCardVm {
    pub(in crate::home) fn new(item: &UserItem, data: Option<&UserItemData>) -> Self {
        let episode_number = match (item.parent_index_number, item.index_number) {
            (Some(season), Some(episode)) => Some(format!("S{season:02}E{episode:02}")),
            (None, Some(episode)) => Some(format!("E{episode:02}")),
            _ => None,
        };
        let title = item
            .series_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&item.name)
            .to_string();
        let subtitle = match episode_number {
            Some(number) if title != item.name => format!("{number} · {}", item.name),
            Some(number) => number,
            None if title != item.name => item.name.clone(),
            None => "单集".to_string(),
        };
        Self {
            title,
            subtitle,
            played_fraction: played_fraction(data),
            played: data.is_some_and(|data| data.played),
        }
    }
}

impl EpisodeCardVm {
    pub(in crate::home) fn new(
        item: &MediaItem,
        data: Option<&UserItemData>,
        selected: bool,
    ) -> Self {
        Self {
            label: item.episode_card_label(),
            overview: compact_episode_overview(item.overview.as_deref()),
            selected,
            played_fraction: selected.then(|| played_fraction(data)).flatten(),
            played: data.is_some_and(|data| data.played),
        }
    }
}

impl From<&MediaPerson> for PersonCardVm {
    fn from(person: &MediaPerson) -> Self {
        Self {
            name: person.display_name(),
            role: person.role_label(),
            kind: person.type_label(),
        }
    }
}

fn played_fraction(data: Option<&UserItemData>) -> Option<f32> {
    data.and_then(|data| data.played_percentage)
        .filter(|percentage| percentage.is_finite())
        .map(|percentage| (percentage.clamp(0.0, 100.0) / 100.0) as f32)
}

fn compact_episode_overview(value: Option<&str>) -> Option<String> {
    let overview = value?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!overview.is_empty()).then_some(overview)
}

pub(in crate::home) fn format_community_rating(rating: f32) -> String {
    let rating = (rating * 10.0).round() / 10.0;
    if rating.fract().abs() < f32::EPSILON {
        format!("{rating:.0}")
    } else {
        format!("{rating:.1}")
    }
}

fn resume_item_card_text(item: &ResumeItem) -> (String, Option<String>) {
    match item.item_type.as_deref() {
        Some("Episode") => {
            let title = item
                .series_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or(&item.name)
                .to_string();
            let subtitle = match (item.parent_index_number, item.index_number) {
                (Some(season), Some(episode)) => format!("S{season}E{episode}: {}", item.name),
                _ => item.name.clone(),
            };

            (title, Some(subtitle))
        }
        Some("Movie") => (
            item.name.clone(),
            item.production_year.map(|year| year.to_string()),
        ),
        _ => (item.name.clone(), None),
    }
}

#[cfg(test)]
mod tests;
